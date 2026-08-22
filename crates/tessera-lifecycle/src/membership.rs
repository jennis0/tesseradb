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

use std::collections::BTreeMap;

use croaring::{Bitmap, Portable};
use tessera_types::EntityId;

/// Execute a layer's `withdraw_on_member_deletion` declaration against one record, for the members
/// this fold retired (`annotation-write-cycle.md` §3.2).
///
/// **An artifact left with no contents is not an artifact with no content** — it is one the
/// serving path withholds, because its layer declares supplied content and it has none to serve.
/// That is [decision 0076](../../../docs/decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)
/// reached from the write side: the alternative is serving the identity and the count with the
/// description missing, which is the in-between state the decision forbids.
///
/// Returns whether the record moved, which is what tells [`ArtifactStore::retire`] whether the
/// level's version has to move with it: a level a fold walked over and did not change has a row
/// form that is still correct, and rebuilding it would be the global grain back again in a
/// narrower place.
fn apply_deletion_policy(record: &mut ArtifactRecord, retired: &Bitmap, withdraw: bool) -> bool {
    match withdraw {
        true => {
            let before = record.contents.len();
            record
                .contents
                .retain(|content| content.generated_from.and_cardinality(retired) == 0);
            record.contents.len() != before
        }
        false => {
            let mut moved = false;
            for content in &mut record.contents {
                if content.generated_from.and_cardinality(retired) == 0 {
                    continue;
                }
                content.generated_from.andnot_inplace(retired);
                moved = true;
            }
            moved
        }
    }
}

/// One level's not-yet-published artifacts, ready to pack: `(layer, level, ordinal_lo, blobs)`.
///
/// A tuple alias rather than a struct because it is a *transfer* between two modules that both
/// already name these four things — a struct would be a third name for the same tuple, and the
/// packer takes them apart again immediately.
pub type PendingExtent = (String, u32, u32, Vec<Vec<u8>>);

/// One artifact as a caller offers it, before the engine has given it an ordinal or an entity.
///
/// **Members are entities, resolved at admission.** A caller names them by `tessera_id` and the
/// control plane inverts them once, at the boundary, exactly as `/control/changes` does — so no
/// blinded identifier reaches durable state, where a key rotation would silently redirect it (I10).
/// An artifact's declared bounding box — the whole of a spatial layer's membership, before it is
/// covered by tiles.
///
/// **The box is content and the tiles are the membership** (ruling R3). What is stored is the box,
/// because it is what the author wrote and what a client is shown; what a request counts is the
/// depth-`d` Morton tiles covering it, resolved against the generation's own segments. Storing the
/// tiles instead would freeze the decomposition against a depth the declaration could later change,
/// and storing the *rows* would freeze it against a geometry the next flush moves.
///
/// **Four `f64`s and no ordering guarantee at this type**: the box is checked where it is published
/// (`LayerRegistry::prepare_artifacts`), so a reader here holds a box that already validated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bbox {
    /// The four values in the order a declaration writes them, or `None` where the box is not one
    /// this crate will store: a non-finite bound, or a maximum below its minimum.
    ///
    /// **Refused rather than normalised.** A box written `[max, min]` is a transposition, and
    /// swapping it silently would serve a membership the author did not write — the covering tiles
    /// of the corrected box, over a region they may not have meant to name at all.
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Option<Self> {
        let finite = [min_x, min_y, max_x, max_y].iter().all(|v| v.is_finite());
        (finite && max_x >= min_x && max_y >= min_y).then_some(Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    /// `[min_x, min_y, max_x, max_y]` — the spelling a declaration and the WAL both use.
    pub fn as_array(&self) -> [f64; 4] {
        [self.min_x, self.min_y, self.max_x, self.max_y]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncomingArtifact {
    /// The caller's own name for this artifact. **Effectively mandatory for a layer another
    /// layer's edges point into**: an edge names its target, and at publish time the caller holds
    /// no `tessera_id` for it.
    pub key: Option<String>,
    pub members: Bitmap,
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
    /// The parent artifact in a hierarchical layer, named by the parent's own key.
    ///
    /// **The lineage is declared upward only, and the downward list is deliberately absent.** A
    /// `children_keys` beside this was read, validated for cross-row agreement, and never walked:
    /// every consumer — containment, coverage, cycle detection, the cut — derives children by
    /// inverting the parent edges, because that is the direction an artifact can state without
    /// knowing what will later point at it. Two spellings of one edge is one more place for them
    /// to disagree.
    pub parent_key: Option<String>,
    /// The artifact's bounding box — required on a layer whose `shape` declares one, refused on
    /// every other kind. It **is** the membership: `members` stays empty on such a layer, because
    /// the tiles covering this box decide who belongs at request time.
    pub shape: Option<Bbox>,
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

impl IncomingContent {
    /// Builds one from resolved entities — the constructor exists for
    /// [`IncomingArtifact::from_entities`]'s reason: `tessera-server` names a set without being
    /// able to do arithmetic on one.
    pub fn new(values: Vec<String>, generated_from: impl IntoIterator<Item = EntityId>) -> Self {
        let mut bitmap = Bitmap::new();
        for entity in generated_from {
            bitmap.add(entity.raw() as u32);
        }
        IncomingContent {
            values,
            generated_from: bitmap,
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
        let mut bitmap = Bitmap::new();
        for entity in members {
            // Entity space is `u32` by I9, so the narrowing is total.
            bitmap.add(entity.raw() as u32);
        }
        IncomingArtifact {
            key,
            members: bitmap,
            contents: Vec::new(),
            attached_to: None,
            parent_key: None,
            shape: None,
        }
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

/// Entities joining an artifact that already exists, as a caller offers them.
///
/// **Addressed by the caller's own key, and resolved on the executor.** An ordinal never crosses
/// the wire (C8) and the caller holds none; the key is the address they published under, and
/// `ArtifactStore::ordinal_of_key` is what resolves it — a lookup in the *store*, never in what is
/// served, so a suppressed artifact resolves like any other and a growth against it leaves it
/// suppressed (`artifacts-from-points.md` §5).
///
/// **Members are entities, resolved at admission**, on [`IncomingArtifact`]'s rule: no blinded
/// identifier reaches durable state, where a key rotation would silently redirect it (I10).
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
}

impl IncomingGrowth {
    /// Builds one from resolved entities — [`IncomingArtifact::from_entities`]'s reason: the
    /// bitmap type stays inside this crate, so a request plane can name a set without being able
    /// to do arithmetic on one.
    pub fn from_entities(key: String, joining: impl IntoIterator<Item = EntityId>) -> Self {
        let mut bitmap = Bitmap::new();
        for entity in joining {
            // Entity space is `u32` by I9, so the narrowing is total.
            bitmap.add(entity.raw() as u32);
        }
        IncomingGrowth {
            key,
            joining: bitmap,
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
    /// `(rank, members of that generating set the fold retired)`, for the contents
    /// that lost any. Empty on a layer that declares no supplied content.
    pub contents_lost: Vec<(u32, u64)>,
}

/// One artifact's durable state, as the registry holds it.
#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    /// This artifact's own entity — its address for the deny lane, and what `tessera_id` blinds.
    pub entity: EntityId,
    /// The caller's own key, if they supplied one. **Effectively mandatory for a layer another
    /// layer's edges point into**: an edge names its target, and at publish time the caller holds
    /// no `tessera_id` for it.
    pub key: Option<String>,
    /// Entity-space membership — the canonical, view-invariant record.
    pub members: Bitmap,
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
    /// This artifact's parent in its layer's hierarchy.
    ///
    /// **The opposite of an attachment in the one way that matters**: it is *not* a visibility term.
    /// A node's verdict is its own masked count against its own criterion, with no input from its
    /// lineage and none from the viewport
    /// ([decision 0080](../../../docs/decisions/0080-the-frontier-is-a-per-artifact-test.md)) — so
    /// a parent that is suppressed, deleted or below its bar withholds itself and nothing else. What
    /// the edge decides is only which of two artifacts that *both* passed is the one drawn.
    ///
    /// A parent whose ordinal is now a hole leaves this node a root, which serves it: correct, since
    /// it passed its own test, and the reason the fold does not have to rewrite these.
    ///
    /// **The level is carried because a layer's edges are one of two shapes.** A nested layer's run
    /// within one level, and the cut climbs them; a tiered layer's run between levels, and
    /// the cut does not — those are information about what contains what, not a ladder to coarsen
    /// along (owner ruling, 2026-08-18).
    pub parent: Option<crate::wal::ParentRef>,
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
    /// Entity-space, canonical. **Empty means corpus-independent** — containment is vacuous and the
    /// content serves to everyone who reaches the layer — and that is a real declaration rather
    /// than a missing one: a layer whose kinds are all corpus-independent is refused a generating
    /// set at publish, so an empty set here cannot be an omission.
    pub generated_from: Bitmap,
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
    /// Per `(layer, level)`, each ordinal's declared bounding box — the membership of a layer whose
    /// `shape` declares one, and `None` everywhere else.
    ///
    /// **Beside the records rather than inside them, on [`ArtifactRecords`]' own split**
    /// (`tessera_engine::artifacts`): the two halves are read at different cadences. A record is
    /// dereferenced on every verdict; a box is read **once per level per generation**, by the pass
    /// that decomposes it into row ranges, and never by a verdict at all. Keeping it out of the
    /// record keeps the type every serving path walks the same shape it was.
    ///
    /// **Written wherever a record is, in [`ArtifactStore::put`], so the two cannot come apart.**
    /// There is no route that sets one without the other, and a level's vectors are grown together;
    /// a box for an ordinal with no record would be a membership rule for an artifact that does not
    /// exist, and a record with no box on a shape layer is refused at publication.
    shapes: BTreeMap<(String, u32), Vec<Option<Bbox>>>,
    /// `(layer, level, key) → ordinal`. **An index, not a second copy of the truth**: it
    /// exists so a batch of ten thousand artifacts can be checked for duplicate keys in
    /// `O(n log n)` rather than rescanning the level per artifact, which is `O(n²)` and reachable
    /// at the sizes this stage publishes.
    keys: BTreeMap<(String, u32, String), u32>,
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
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace one artifact, and the bounding box it declared if its layer declares a
    /// shape. Growing the level's vectors to fit is what makes a publication that arrives out of
    /// ordinal order land correctly.
    ///
    /// **One call sets both halves** — see [`ArtifactStore::shapes`] for why the box is beside the
    /// record rather than in it, and why there is no route that writes one without the other.
    pub fn put(
        &mut self,
        layer: &str,
        level: u32,
        ordinal: u32,
        record: ArtifactRecord,
        shape: Option<Bbox>,
    ) {
        if let Some(key) = &record.key {
            self.keys
                .insert((layer.to_string(), level, key.clone()), ordinal);
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
        let boxes = self.shapes.entry((layer.to_string(), level)).or_default();
        if boxes.len() <= idx {
            boxes.resize(idx + 1, None);
        }
        boxes[idx] = shape;
    }

    /// The bounding box the artifact at `ordinal` declared, or `None` where it declared none —
    /// which is every artifact of every layer whose membership is not a shape.
    pub fn shape_of(&self, layer: &str, level: u32, ordinal: u32) -> Option<Bbox> {
        self.shapes
            .get(&(layer.to_string(), level))
            .and_then(|boxes| boxes.get(ordinal as usize))
            .copied()
            .flatten()
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

    /// The ordinal a caller's own key names in this level, if any.
    pub fn ordinal_of_key(&self, layer: &str, level: u32, key: &str) -> Option<u32> {
        self.keys
            .get(&(layer.to_string(), level, key.to_string()))
            .copied()
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
    /// **Two bounds, one answer.** The oldest unpacked publication, and the oldest growth no whole
    /// rewrite has covered — the minimum of the two, because rotation takes a single bound and the
    /// two are released by different events (`grown_wal_pos`).
    pub fn oldest_wal_pos(&self) -> Option<u64> {
        match (self.oldest_wal_pos, self.grown_wal_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Applies a durable publication or a durable growth — the **two** paths by which memberships
    /// enter, taken by both the live write path and replay.
    ///
    /// `position` is where the record sits in the log. **Replay applies the recorded ordinals and
    /// entities rather than re-deriving them**, on [`crate::LayerRegistry::apply`]'s contract: a
    /// re-derived ordinal would move an artifact under every suppression naming it.
    ///
    /// Records other than these two are ignored, so a caller can hand the whole replay stream to
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
            _ => 0,
        }
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
            // localised to the one content and skipped.
            let sets: Option<Vec<ContentSet>> = published
                .contents
                .iter()
                .map(|v| {
                    deserialise_members(&v.generated_from).map(|generated_from| ContentSet {
                        values: Some(v.values.clone()),
                        generated_from,
                    })
                })
                .collect();
            let Some(contents) = sets else {
                refused += 1;
                continue;
            };
            let shape = published
                .shape
                .and_then(|b| Bbox::new(b[0], b[1], b[2], b[3]));
            self.put(
                layer,
                level,
                published.ordinal,
                ArtifactRecord {
                    entity: published.entity,
                    key: published.key.clone(),
                    members,
                    contents,
                    attached_to: published.attached_to.clone().map(|a| Attachment {
                        layer: a.layer,
                        level: a.level,
                        ordinal: a.ordinal,
                        entity: a.entity,
                    }),
                    parent: published.parent,
                },
                shape,
            );
        }
        self.oldest_wal_pos = Some(match self.oldest_wal_pos {
            Some(existing) => existing.min(position),
            None => position,
        });
        self.bump(layer, level);
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
        record.members.or_inplace(joining);
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
        }
        self.levels.retain(|(l, _), _| l != layer);
        self.keys.retain(|(l, _, _), _| l != layer);
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

    /// Every level [`Self::retire`] would move, given the same `retired` set — the read-only twin
    /// of its `changed`, and it lives beside it so the two are read together.
    ///
    /// **Conservative where it is not exact.** A level is reported whenever anything about it
    /// *could* change: an artifact whose own entity is retired, a membership that loses a bit, or a
    /// generating set that loses a member — which is what both deletion policies key on. Reporting
    /// a level that would not in fact have moved costs a derived structure that is recomposed;
    /// missing one that would have moved is a structure adopted against records it does not
    /// describe, and that is the direction this must not fail in.
    ///
    /// **Why a publication needs it at all.** A fold writes its manifest *before* it retires — the
    /// retirement is not reversible and a manifest that would not commit must leave it undone — so
    /// the version this store reports at that moment is the pre-retirement one while the records
    /// the manifest names are the post-retirement ones. For a level the retirement moves, those two
    /// facts do not belong in one manifest, so neither is stated: see `write.rs`'s
    /// `artifact_coordinates`.
    pub fn levels_moved_by(&self, retired: &Bitmap) -> Vec<(String, u32)> {
        if retired.is_empty() {
            return Vec::new();
        }
        let mut moved = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let touched = slots.iter().flatten().any(|record| {
                retired.contains(record.entity.raw() as u32)
                    || record.members.and_cardinality(retired) != 0
                    || record
                        .contents
                        .iter()
                        .any(|content| content.generated_from.and_cardinality(retired) != 0)
            });
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
            let tail = &slots[from..];
            if tail.iter().any(Option::is_none) {
                skipped.push((layer.clone(), *level));
                continue;
            }
            let blobs: Vec<Vec<u8>> = tail
                .iter()
                .enumerate()
                .map(|(i, slot)| {
                    encode_record(
                        slot.as_ref().expect("checked dense just above"),
                        self.shape_of(layer, *level, (from + i) as u32),
                    )
                })
                .collect();
            ready.push((layer.clone(), *level, from as u32, blobs));
        }
        (ready, skipped)
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
    ///   no longer exists. Under the layer's strict declaration the content and its set are dropped
    ///   at this fold; under permissive the member leaves the set and it serves again. Either way
    ///   the caller is owed the notice, because only they can decide whether the text still says
    ///   something true.
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
    /// **Generating sets move only where the layer said they may**, which is `policy`
    /// (`annotation-write-cycle.md` §3.2). Under `WithdrawContent` — the default — a content that
    /// lost a source is **dropped whole**, content and set together, because containment is
    /// all-or-nothing and a set that lost a member fails it for every principal for ever; the caller
    /// regenerates. Under `ShrinkGeneratingSet` the member leaves the set and the content serves
    /// again, which is a channel the caller chose for an object whose membership is statistical.
    /// Neither is a service *behaviour*: both are the declaration executing.
    ///
    /// A level with a hole is reported rather than packed around, exactly as in
    /// [`Self::unpublished`] — but the consequence differs and the caller must not treat it as a
    /// skip: an extent this rewrite omits is a level the new prefix does not carry at all, whose
    /// artifacts come back registered, addressable and served as absent.
    pub fn repack_all(
        &self,
        retired: &Bitmap,
        policy: &dyn Fn(&str) -> bool,
    ) -> Vec<PendingExtent> {
        let mut ready = Vec::new();
        for ((layer, level), slots) in &self.levels {
            if slots.is_empty() {
                continue;
            }
            let on_deletion = policy(layer);
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
                    // **The box survives a fold unchanged**, and that is what makes a shape
                    // layer's membership fold-invariant: a box is geometry, not rows, so nothing
                    // the fold renumbers reaches it. What a deletion removes from such a layer is
                    // the *point*, which leaves the mask — the artifact's rule is untouched.
                    let shape = self.shape_of(layer, *level, ordinal as u32);
                    if retired.is_empty() {
                        return encode_record(record, shape);
                    }
                    let mut record = record.clone();
                    record.members.andnot_inplace(retired);
                    let _ = apply_deletion_policy(&mut record, retired, on_deletion);
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
    /// **Only the levels this actually changed have their version moved.** A fold walks every
    /// level and most folds touch few of them; bumping the ones it read would be the global grain
    /// [`Self::versions`] exists to escape, in the one place where it is least affordable.
    pub fn retire(&mut self, retired: &Bitmap, policy: &dyn Fn(&str) -> bool) {
        if retired.is_empty() {
            return;
        }
        // Collected during the walk and applied after it: both indexes are fields beside `levels`,
        // which is borrowed mutably here.
        let mut gone: Vec<(EntityId, EntityId)> = Vec::new();
        let mut moved: Vec<(String, u32)> = Vec::new();
        for ((layer, level), slots) in self.levels.iter_mut() {
            let on_deletion = policy(layer);
            let mut changed = false;
            for slot in slots.iter_mut() {
                let Some(record) = slot else { continue };
                if retired.contains(record.entity.raw() as u32) {
                    if let Some(key) = &record.key {
                        self.keys.remove(&(layer.clone(), *level, key.clone()));
                    }
                    if let Some(attachment) = &record.attached_to {
                        gone.push((attachment.entity, record.entity));
                    }
                    *slot = None;
                    changed = true;
                    continue;
                }
                // Asked before the removal rather than compared after it: `and_cardinality` is the
                // same walk over the same containers `andnot_inplace` takes, and it answers the
                // question the version needs without a second copy of the membership to compare
                // against.
                let shrinks = record.members.and_cardinality(retired) != 0;
                record.members.andnot_inplace(retired);
                changed |= shrinks;
                changed |= apply_deletion_policy(record, retired, on_deletion);
            }
            if changed {
                moved.push((layer.clone(), *level));
            }
        }
        for (layer, level) in moved {
            self.bump(&layer, level);
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
    pub fn unpublished_content(&self) -> Vec<(EntityId, Vec<(u16, String)>)> {
        let mut out = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let from = *self
                .published_through
                .get(&(layer.clone(), *level))
                .unwrap_or(&0) as usize;
            if from >= slots.len() {
                continue;
            }
            for slot in &slots[from..] {
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
        shape: Option<Bbox>,
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
///             | u16 LE content_count
///             | u32 LE members_len | membership bytes (portable Roaring)
///             | content*
///             | attachment
///             | parent
///             | shape
/// content    := u32 LE set_len | generating-set bytes (portable Roaring)
/// attachment := u8 0                                     -- unattached
///             | u8 1 | u16 LE layer_len | layer bytes (UTF-8)
///                    | u32 LE level | u32 LE ordinal | u64 LE target entity
/// parent     := u8 0                                     -- a root
///             | u8 1 | u32 LE level | u32 LE ordinal
/// shape      := u8 0                                     -- no declared box
///             | u8 1 | f64 LE min_x | f64 LE min_y | f64 LE max_x | f64 LE max_y
/// ```
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
/// **The key travels with the membership because nothing else durable carries it.** An artifact's
/// entity is derivable from its layer's reserved runs and its ordinal, so the extent need not carry
/// it; a caller's key is derivable from nothing. Putting it in the manifest instead would put
/// one JSON string per artifact in a document parsed at every open — the entry-count problem the
/// packing exists to solve, in another guise.
///
/// `tessera-store` holds this as an opaque blob and addresses it by ordinal. **That split is the
/// layering**: the store owns which bytes belong to which artifact, this crate owns what the bytes
/// mean, and the bitmap library stays on one side of the boundary.
pub fn encode_record(record: &ArtifactRecord, shape: Option<Bbox>) -> Vec<u8> {
    let key = record.key.as_deref().unwrap_or_default().as_bytes();
    let members = serialise_members(&record.members);
    let sets: Vec<Vec<u8>> = record
        .contents
        .iter()
        .map(|v| serialise_members(&v.generated_from))
        .collect();
    let mut out = Vec::with_capacity(
        8 + key.len() + members.len() + sets.iter().map(Vec::len).sum::<usize>(),
    );
    // A key longer than a `u16` cannot round-trip, and truncating one would silently rename an
    // artifact. The control plane bounds the request body long before this, so the clamp is a
    // backstop; it refuses at encode rather than writing a key it cannot read back.
    let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&key_len.to_le_bytes());
    if key_len != u16::MAX {
        out.extend_from_slice(key);
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
        for set in &sets {
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
    // The parent, on the attachment's discriminant rule: absent is one byte and never zero, so
    // *this node is a root* and *this reader does not know whether it had a parent* cannot encode
    // the same. Here the second answer would serve a child beside the ancestor that should have
    // replaced it — a duplicate on the map rather than a disclosure, but wrong either way.
    match record.parent {
        None => out.push(0),
        Some(parent) => {
            out.push(1);
            out.extend_from_slice(&parent.level.to_le_bytes());
            out.extend_from_slice(&parent.ordinal.to_le_bytes());
        }
    }
    // **The box, on the same discriminant rule** — and here the fail-closed reading is the loud
    // one. A spatial artifact restored *without* its box has no membership rule at all, so it
    // counts zero for every viewer and is absent under any criterion; the decoder refuses such a
    // blob rather than restoring a shapeless artifact, which is what makes the absence a fault
    // somebody sees instead of a boundary that quietly stopped holding anything.
    match shape {
        None => out.push(0),
        Some(box_) => {
            out.push(1);
            for value in box_.as_array() {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    out
}

/// The inverse, refusing anything it cannot read back exactly.
///
/// **A refusal and never a partial record**, on [`deserialise_members`]'s argument: an artifact whose
/// key was lost is one no edge can name, and an artifact whose membership decoded short is one with
/// a low masked count for every viewer — which the existence criterion renders as *absent*, with no
/// error anywhere to notice. Both must be a decode failure the caller alarms on.
pub fn decode_record(entity: EntityId, blob: &[u8]) -> Option<(ArtifactRecord, Option<Bbox>)> {
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
    let count = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if count == u16::MAX as usize {
        return None;
    }
    let members_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let members = deserialise_members(take(members_len)?)?;
    let mut contents = Vec::with_capacity(count);
    for _ in 0..count {
        let set_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        contents.push(ContentSet {
            // ⊘ The extent carries no values — see [`ContentSet::values`]. A restored content is
            // therefore unservable until the blob write lands, which is fail-closed and loud rather
            // than an artifact served with its description missing.
            values: None,
            generated_from: deserialise_members(take(set_len)?)?,
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
    let parent = match take(1)?[0] {
        0 => None,
        1 => Some(crate::wal::ParentRef {
            level: u32::from_le_bytes(take(4)?.try_into().ok()?),
            ordinal: u32::from_le_bytes(take(4)?.try_into().ok()?),
        }),
        _ => return None,
    };
    // **The box goes through [`Bbox::new`] rather than being assembled from the bytes**, so a blob
    // carrying an inverted or non-finite box is a decode failure and not an artifact whose
    // membership is a region nobody wrote. One constructor, at both ends.
    let shape = match take(1)?[0] {
        0 => None,
        1 => {
            let mut values = [0f64; 4];
            for value in values.iter_mut() {
                *value = f64::from_le_bytes(take(8)?.try_into().ok()?);
            }
            Some(Bbox::new(values[0], values[1], values[2], values[3])?)
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
            members,
            contents,
            attached_to,
            parent,
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
            members: Bitmap::of(members),
            contents: Vec::new(),
            attached_to: None,
            parent: None,
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
        store.retire(&Bitmap::of(&[200]), &|_| false);
        assert!(
            store.cascade_from(&[EntityId::new(100)]).is_empty(),
            "the label is gone, so deleting its cluster cascades into nothing"
        );
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
        r.members.remove(3);
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

    /// **An attachment survives the packed extent, and a lost one is a decode failure.** A label
    /// restored as unattached is a label that serves when its cluster is suppressed — the fail-open
    /// the term exists to close, reappearing at a restart, with nothing anywhere reporting a fault.
    #[test]
    fn an_attachment_round_trips_and_a_truncated_one_is_refused() {
        let mut r = record(100, &[1, 2, 3]);
        r.key = Some("l0".into());
        r.contents = vec![ContentSet {
            values: Some(vec!["a label".into()]),
            generated_from: Bitmap::of(&[1, 2]),
        }];
        r.attached_to = Some(Attachment {
            layer: "clusters/a".into(),
            level: 0,
            ordinal: 17,
            entity: EntityId::new(4_294_901_759),
        });

        let shape = Bbox::new(-1.5, 0.0, 2.5, 4.0);
        let blob = encode_record(&r, shape);
        let (back, back_shape) = decode_record(r.entity, &blob).expect("a whole blob decodes");
        assert_eq!(back.attached_to, r.attached_to);
        assert_eq!(back.key, r.key);
        assert_eq!(
            back_shape, shape,
            "the box is the membership of a shape layer"
        );

        // An unattached artifact with no box round-trips too, each carrying its own absence byte —
        // *unattached* and *this reader could not tell* must not encode the same, and neither must
        // *no box* and *a box this reader could not read*.
        let mut plain = r.clone();
        plain.attached_to = None;
        let plain_blob = encode_record(&plain, None);
        let (restored, restored_shape) = decode_record(plain.entity, &plain_blob).unwrap();
        assert_eq!(restored.attached_to, None);
        assert_eq!(restored.members, plain.members);
        assert_eq!(restored_shape, None);

        // **An inverted box is a decode failure, not a swapped one.** The blob is written by hand
        // here because `Bbox::new` refuses to build one — which is the point: the only way such a
        // blob exists is a writer that did not go through the constructor, and the reader must not
        // accept what the writer could not have produced.
        let mut inverted = encode_record(&plain, None);
        inverted.pop();
        inverted.push(1);
        for value in [5.0f64, 0.0, 1.0, 4.0] {
            inverted.extend_from_slice(&value.to_le_bytes());
        }
        assert!(
            decode_record(plain.entity, &inverted).is_none(),
            "a box whose maximum is below its minimum names a region nobody wrote"
        );

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
                members: serialise_members(&Bitmap::of(members)),
                contents: Vec::new(),
                attached_to: None,
                parent: None,
                shape: None,
            }],
        }
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
        store.retire(&Bitmap::of(&[100]), &|_| false);

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

        store.retire(&Bitmap::of(&[3]), &|_| false);
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
}
