//! Annotation layers, and the artifacts in them, declared as **build inputs**.
//!
//! A layer registered on the control plane and a layer written by a build are the same object:
//! both end as a `RegisteredLayer` in `SEGMENTS-<n>.json`, and the engine seeds its registry from
//! that section before it replays a single WAL record. What this module adds is the route — the
//! config's `[[layer]]` blocks and the sources they name, so a bundle comes up with its layers
//! already there. **Each layer names its own source** — a Parquet of one row per artifact, or the
//! rows written into the declaration itself — so no row anywhere carries the layer it belongs to.
//!
//! **Why the build plane exists for this at all.** A 10⁷-artifact level is a build job for the same
//! reason `--attach-view` is: volume that must not ride the trickle path, where every batch is an
//! fsync and the log is pinned from the first publication until a manifest carries it
//! (`annotation-representation.md` §5.0). The control plane stays the route for a correction, an
//! interactive selection, and anything that must take effect against a running node.
//!
//! ## One implementation of the rules, not two
//!
//! Every validation and every allocation here runs through
//! [`LayerRegistry`](tessera_lifecycle::LayerRegistry) and
//! [`Allocator`](tessera_lifecycle::alloc::Allocator) — the same calls
//! `PUT /control/layers` makes, in the same order, producing the same WAL records, which this
//! module then throws away because a build's durable output is its manifest. So a declaration
//! refused online is refused here with the same words, an artifact's ordinals and entities come out
//! where the control plane would have put them, and the two routes cannot drift into disagreeing
//! about what a layer is. The one rule that has no build-plane meaning is publish-time member
//! validation against the deny lane (`annotation-write-cycle.md` §3.1): a bundle straight out of
//! `tessera build` has no overlay, so no declared member can be deleted or suppressed yet.
//!
//! ## Addressing: source ids, because that is what a build input has
//!
//! Members are named by the **source** entity id — the `entity_id` of the points file — and
//! resolved through this build's own assignment, exactly as the pairs file's ids are. A
//! `tessera_id` would be meaningless: it is a keyed permutation of an entity space this build is in
//! the middle of assigning. An id the build did not assign **refuses the build** rather than being
//! dropped, on `input`'s rule for the pairs file and for the sharper reason the control plane gives
//! it: a dropped member moves both the count a viewer is shown and the size a proportional
//! criterion divides by, quietly, in the direction of hiding an artifact.
//!
//! ## Determinism
//!
//! Ordinals are identity (an artifact's entity is `run.start + ordinal`), so their assignment may
//! not depend on the order rows happen to sit in a Parquet file. Artifacts are therefore published
//! in `(layer, level, key)` order, and a key is **required** for a build-published artifact — the
//! caller's own name for it is the only address that survives a rebuild, and it is what an edge
//! into the layer names.
//!
//! ## What the batched publication changed about `attached_to`
//!
//! A level is published in batches sized by the memory budget ([`publication_batch_entries`]), and
//! `prepare_publish` resolves a parent against the batch in hand **plus the store**. So an
//! `attached_to` naming an artifact in an *earlier batch of the same level* now resolves, where an
//! unbatched publication refused it as a within-level edge. Whether that shape is accepted
//! therefore depends on the budget the build ran under, which is not a property a declaration
//! should have.
//!
//! Recorded rather than acted on. Levels carrying within-level hierarchy edges are published whole
//! ([`within_level_edges`]) precisely so the resolution does not depend on the cut; what is left is
//! that a level *without* declared within-level edges can still carry one through `attached_to`
//! and have it resolve or not by budget. The fix is a refusal at validation, and it is an owner's
//! call whether the shape is refused at all.
//!
//! ## The declarations are not read here
//!
//! [`crate::config`] parses them, out of the one document that also carries the attributes, the
//! vocabularies and the views. What is left in this module is the *data* path: each layer's own
//! artifacts and members, the publication order, and the hierarchy checks that need every artifact
//! in hand.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{
    Array, FixedSizeListArray, Int16Array, Int32Array, Int64Array, Int8Array, ListArray,
    StringArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use tessera_lifecycle::alloc::Allocator;
use tessera_lifecycle::membership::{
    ArtifactShapes, ArtifactStore, IncomingArtifact, IncomingAttachment, IncomingContent,
};
use tessera_lifecycle::LayerRegistry;
use tessera_store::manifest::{MembershipExtent, RecordExtent};
use tessera_types::layer::RegisteredLayer;
use tessera_types::layer::{parent_edges, LayerDeclaration, ListMeaning, ValueSet};
use tessera_types::EntityId;

use rayon::prelude::*;

use crate::config::{ArtifactSource, Fields, InlineArtifact, LayerSources};
use crate::error::{BuildError, Result};
use crate::shapes::{
    inline_shape, shape_declared, space_at, space_column, ShapeColumns, ShapeContext,
    ShapeLayerReport, ShapeReader,
};
use crate::spill;
use tessera_store::derived::authored_shape_input;

/// One artifact as the build inputs describe it, before any id has been resolved.
#[derive(Debug, Default)]
struct PlannedArtifact {
    /// The key of the view this artifact belongs to, on a layer scoped to a group
    /// (`views.md` §3.5); `None` on an unscoped layer, whose one artifact set is drawn on every
    /// view it names.
    view_key: Option<String>,
    membership: PlannedMembership,
    /// Indexed by rank, dense — a gap would silently renumber the caller's ranking.
    contents: Vec<PlannedContent>,
    attached_to: Option<IncomingAttachment>,
    /// Parent artifacts in a hierarchy, each named by the parent's own key — one on a tree, and on
    /// a `dag` layer as many as the artifact sits beneath (`dag-hierarchies.md` §4). Each key once.
    parent_keys: Vec<String>,
    /// The artifact's canonical shapes, one per view, on a layer whose `shape` declares one —
    /// which *is* its membership there, so `membership` stays empty beside it.
    shape: Option<ArtifactShapes>,
    /// The row's own `space`, as it was written, overriding the table's `default_space`
    /// (`polygon-membership.md` §4.3). Kept past the membership shape because the row's authored
    /// shape content is read in the same space, and is read in a second pass once every row of the
    /// layer is in hand.
    space: Option<String>,
}

/// How a source spelled one artifact's membership.
///
/// **The only place the exclusion spelling exists, and it ends at [`resolve_artifact`].** A
/// membership declared by exclusion is complemented once, against the entity space this build
/// assigned, and everything downstream of that call — the store, the packed extents, the manifest,
/// every read path — receives the same materialised set an inclusion would have produced. That is
/// what makes *no request-time complement* structural rather than a rule to remember: there is no
/// type below this one that can carry the spelling, so no serving path can learn it and none can
/// evaluate a complement against a viewer's mask, which would disclose the existence of items
/// outside it (`annotation-write-cycle.md` §6.1).
#[derive(Debug, Clone, Default)]
enum PlannedMembership {
    /// Rows from a `[layer.members]` source — **accumulated in [`MemberSpill`] and not here**, so
    /// an artifact a hundred million rows name costs the plan nothing. Also the membership of an
    /// artifact no source named at all, which is the empty one.
    #[default]
    Rows,
    /// The artifact row's own `members` list.
    Included(Vec<u64>),
    /// The artifact row's `excluding` list: the entities the membership leaves out.
    Excluded(Vec<u64>),
}

#[derive(Debug, Default, Clone)]
struct PlannedContent {
    values: Vec<String>,
    generated_from: Vec<u64>,
}

/// One artifact with every source id resolved to the entity this build assigned it, and every
/// membership materialised — the complement included.
#[derive(Debug)]
struct ResolvedArtifact {
    /// The view this artifact belongs to on a group-scoped layer (`views.md` §3.5) — the row's
    /// own `view` column, carried to the record rather than to a side map: the store holds one
    /// shape for the build's artifacts and the wire's (`ArtifactRecord::view`).
    view: Option<String>,
    members: ResolvedMembers,
    contents: Vec<IncomingContent>,
    attached_to: Option<IncomingAttachment>,
    parent_keys: Vec<String>,
    shape: Option<ArtifactShapes>,
}

/// Where an artifact's members are by the time anything wants to read them.
///
/// **The two spellings have different sizes and are held differently, and that is the whole of the
/// distinction.** A membership from a `[layer.members]` source is one row per member, so a corpus
/// of 5×10⁸ member rows has 5×10⁸ of them and no build may hold them; it goes through the spill
/// and lives in the merged [`spill::MemberTable`], read back one artifact at a time. A membership
/// spelled on the artifact's *own row* is one list per artifact in a file an author wrote, so it
/// is held where it was read.
///
/// The members are raw entity ids rather than [`EntityId`] because a source id and the entity it
/// resolves to are both `u64`, so the inline case is rewritten in place rather than copied. The
/// newtype goes back on at [`incoming_artifact`], the one place these leave this module.
#[derive(Debug)]
enum ResolvedMembers {
    /// At this artifact's extent of the merged member table.
    Table,
    /// Materialised here: the artifact row's `members`, or the complement of its `excluding`.
    Inline(Vec<u64>),
}

/// What the build reads: declarations, and the artifacts to publish into them.
pub struct LayerPlan {
    declarations: Vec<LayerDeclaration>,
    /// Per shape layer, what its geometry is (`polygon-membership.md` §6.5) — printed by the
    /// build beside the layer, and completed by the artifact pass with the resolution's cost.
    pub shape_reports: Vec<ShapeLayerReport>,
    /// Keyed `(layer, level, key)`, which is also the publication order — see the module doc on
    /// determinism. **The value is an index into [`Self::bodies`], not the artifact itself**: the
    /// per-point path resolves a key to that index once and then writes through it, where holding
    /// the artifact here made every member entry a `BTreeMap` probe whose key comparison walks two
    /// heap `String`s. At the Overture rung that was three probes and two allocations per entry
    /// over 3×10⁸ entries.
    ///
    /// The map is still what publication order is read off, so ordinals — which are identity under
    /// I9 — remain a function of the keys and never of arena order.
    artifacts: BTreeMap<(String, u32, String), usize>,
    /// The artifacts themselves, named by the index [`Self::artifacts`] carries. Append-only: an
    /// index handed out stays valid for the whole read.
    bodies: Vec<PlannedArtifact>,
    /// The address of each body, at the same index — so a resolved key can be carried as one
    /// `usize` and still name itself when a refusal has to quote it.
    addresses: Vec<Address>,
    /// Member rows whose key said *this point is in no artifact*, per source.
    unclustered: Vec<UnclusteredRows>,
    /// How many artifacts each layer's member source **created** — a key the artifacts source did
    /// not declare, under `value_set = "open"` (`artifacts-from-points.md` §3). Counted because a
    /// typo creates a permanent object rather than being refused, which is the trade open makes
    /// knowingly, and the mitigation is that the number is printed. The wire says the same thing in
    /// its own 200.
    minted: BTreeMap<String, u64>,
    /// Every member row's `(artifact, source)` pair, on its way to disk.
    members: MemberSpill,
    /// The build's memory budget, which is what the publication batch is sized against
    /// ([`PUBLICATION_BUDGET_SHARE`]).
    memory_budget: u64,
}

/// What one `(artifact, source)` pair is charged against the accumulator's budget: eight bytes of
/// `u64` at sixteen, because a `Vec` grows by doubling and is on average half empty. The text
/// index charges its postings on the same rule and for the same reason.
const MEMBER_ENTRY_BYTES: usize = 16;

/// What one artifact costs the accumulator beyond its members: its slot in the window, the
/// allocation its first source takes and the allocator's rounding on both. **An estimate erring
/// high**, which spills a run early; erring low is the failure a memory bound exists to prevent.
const MEMBER_ARTIFACT_BYTES: usize = 80;

/// The share of the build's memory budget the member accumulator may hold, and its floor and
/// ceiling. Sixteenth, floor and ceiling all follow the text index's, which is the other pass in
/// this build that spills sorted runs — one rule rather than two constants to keep in step.
///
/// ⊘ Deliberately **not** added to `residency.rs`'s model, for that pass's reason: it is a
/// sixteenth of the same budget the model is checked against, inside the factor of two that module
/// states as its own error bar, and adding it would turn builds that fit today into refusals.
const MEMBER_BUDGET_SHARE: u64 = 16;
const MEMBER_BUDGET_MIN: u64 = 64 << 20;
const MEMBER_BUDGET_MAX: u64 = 1 << 30;

/// The most runs one merge opens at once. A run is a file descriptor and a 4 MiB read buffer, so
/// this is what the merge's own residency is a function of. The text index's number, for the same
/// reason it has one.
const MEMBER_MERGE_FAN_IN: usize = 128;

/// Every layer's member rows, accumulated as `(artifact, source)` pairs and spilled as **sorted
/// runs** once the accumulator reaches its budget.
///
/// **The plan used to hold all of them, and that was the build's largest single term.** One
/// `Vec<u64>` per artifact, grown a member at a time as the member table was read, all of them
/// live from the first row of the first source until the last level was published — 4 GB at the
/// Overture rung, and linear in the corpus with nothing to bound it. Halving the constant (the
/// resolution now rewrites in place rather than copying) left it linear.
///
/// So the pairs go to disk instead, on the shape the text index proved: accumulate to a budget,
/// spill a sorted run, and merge the runs into per-artifact contiguity at the point of use. What
/// is resident here is the budget, whatever the corpus is; the run count grows instead of the
/// peak.
///
/// **Sorted by artifact, and within an artifact by source.** The artifact order is what the merge
/// needs. The source order is what makes a run small — a membership's sources are dense in the
/// corpus's id space, so their deltas are overwhelmingly one byte where an absolute id is five to
/// ten — and it costs one sort of a slice that is usually already ascending, the member tables
/// this reads being written in entity order.
///
/// **Duplicates survive.** The same document named twice for one artifact is two member entries,
/// and the containment report counts entries; a spill that deduplicated would silently change a
/// number an operator is given.
struct MemberSpill {
    /// `.build-tmp/`, the build's own transient directory — so a killed build leaves nothing for
    /// the next one to trip over. `TmpDir` deletes a stale directory at creation and its own on
    /// drop.
    dir: PathBuf,
    budget: usize,
    bytes: usize,
    /// The open window: one slot per artifact the plan has minted, holding the sources this window
    /// has seen for it. Indexed rather than keyed, because the index *is* the artifact's position
    /// in the plan and this is written once per member entry — 2.95×10⁸ of them over the GBIF
    /// ladder corpus, where hashing the index cost more than the write it addressed
    /// (`probes/2026-09-09-layers-cost/`). The slots stay and each one's sources are handed to the
    /// run at the spill, so what a window holds is released with it; what does not is one empty
    /// `Vec` header per artifact.
    open: Vec<Vec<u64>>,
    receipts: Vec<spill::SpillReceipt>,
    seq: usize,
    /// Every pair ever pushed, across every run — what the merge checks itself against and what
    /// the stage reports.
    entries: u64,
}

impl MemberSpill {
    fn new(dir: &Path, budget: u64) -> MemberSpill {
        let budget = budget / MEMBER_BUDGET_SHARE;
        MemberSpill {
            dir: dir.to_path_buf(),
            budget: budget.clamp(MEMBER_BUDGET_MIN, MEMBER_BUDGET_MAX) as usize,
            bytes: 0,
            open: Vec::new(),
            receipts: Vec::new(),
            seq: 0,
            entries: 0,
        }
    }

    fn push(&mut self, index: usize, source: u64) -> Result<()> {
        // Entity space is `u32` by I9 and an artifact is an entity, so an index this cannot hold
        // is a plan no build could publish anyway — refused here, where the number is still in
        // hand, rather than as a truncation on disk.
        let index = u32::try_from(index).map_err(|_| {
            BuildError::Invalid(format!(
                "this build planned more than {} artifacts, which is more than an entity space \
                 can address",
                u32::MAX
            ))
        })? as usize;
        if index >= self.open.len() {
            self.open.resize_with(index + 1, Vec::new);
        }
        let sources = &mut self.open[index];
        // Empty is *this window has not seen the artifact yet*, whether it was never seen or its
        // sources went out with the last run — which is the window the artifact charge belongs to.
        if sources.is_empty() {
            self.bytes += MEMBER_ARTIFACT_BYTES;
        }
        sources.push(source);
        self.bytes += MEMBER_ENTRY_BYTES;
        self.entries += 1;
        if self.bytes >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    /// Write the open window out as one sorted run, leaving the accumulator empty.
    fn spill(&mut self) -> Result<()> {
        if self.bytes == 0 {
            return Ok(());
        }
        let path = self.dir.join(format!("member-run-{:04}.spill", self.seq));
        let mut writer = spill::MemberRunWriter::create(&path)?;
        // Ascending by artifact index, which the slots already are — the run's own order, taken by
        // walking the window rather than by sorting a key set out of it.
        for (index, slot) in self.open.iter_mut().enumerate() {
            if slot.is_empty() {
                continue;
            }
            let mut sources = std::mem::take(slot);
            sources.sort_unstable();
            writer.push(index as u32, &sources)?;
        }
        self.receipts.push(writer.finish()?);
        self.bytes = 0;
        self.seq += 1;
        Ok(())
    }

    /// Spill the tail and hand over every run — the accumulator is empty afterwards.
    fn finish(&mut self) -> Result<Vec<spill::SpillReceipt>> {
        self.spill()?;
        Ok(std::mem::take(&mut self.receipts))
    }
}

impl LayerPlan {
    /// The index of `address`, minting an empty artifact for it if the plan does not hold one.
    ///
    /// **Minted into the plan, never over it**: an address already present keeps the artifact it
    /// has, members and all.
    fn intern(&mut self, address: Address) -> usize {
        let next = self.bodies.len();
        match self.artifacts.entry(address.clone()) {
            std::collections::btree_map::Entry::Occupied(e) => *e.get(),
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(next);
                self.bodies.push(PlannedArtifact::default());
                self.addresses.push(address);
                next
            }
        }
    }

    fn address_of(&self, index: usize) -> &Address {
        &self.addresses[index]
    }
}

/// How many rows of one member source named no artifact.
///
/// **Skipped, counted and printed** (`artifacts-from-points.md` §2, §7). A condensed tree drops a
/// fifth to a quarter of its points as noise at each split, so refusing a null or `-1` key would
/// fail the build on the ordinary output of every clusterer — and dropping them silently is the
/// failure this build has shipped once already. The number is the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnclusteredRows {
    pub layer: String,
    pub source: PathBuf,
    /// Rows carrying a null key, or exactly `-1`.
    pub rows: u64,
}

/// One parent/child edge whose child holds a member its parent does not.
///
/// **A report, not a refusal.** Containment is what makes rollup sound under an absolute criterion
/// — a child's masked count can never exceed its parent's, so a passing child never sits beneath a
/// failing parent — and an edge that breaks it silently withdraws that guarantee for its branch.
/// Naming the edge at build time is what lets an operator see it before a viewer does; deciding
/// what to do about it is theirs, since a corpus may legitimately carry one (an analysis rerun
/// against a moved corpus, a hand-corrected assignment).
///
/// **Not to be confused with a non-covering hierarchy**, which is not a violation at all: HDBSCAN's
/// children are subsets of their parents and do *not* exhaust them, 20–25% of a parent's members
/// falling out as noise at each split. Stray members in the parent are the normal case; members in
/// the child that the parent lacks are this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentViolation {
    pub layer: String,
    pub level: u32,
    pub child: String,
    pub parent: String,
    /// How many of the child's members its parent does not hold.
    pub escaping_members: u64,
}

/// How much of one parent's membership its children between them hold.
///
/// **The normal case is that they do not hold all of it**, and this is the report that says so in
/// advance. HDBSCAN loses a fifth to a quarter of a parent's points as noise at each split, so a
/// parent keeps members no child holds — and those members are the ones that make the parent
/// visible *alone*, with none of its children, to a principal who can see them and nothing else.
/// That is a correct answer and a surprising one, and an operator should meet it here rather than
/// in a support question about why a cluster has no children on the map.
///
/// It decides nothing. A split that loses nine tenths of its parent is published exactly as one
/// that loses none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitCoverage {
    pub layer: String,
    pub level: u32,
    pub parent: String,
    pub children: u32,
    /// The parent's own membership size.
    pub members: u64,
    /// How many of them no child holds — the stray.
    pub stray_members: u64,
}

/// One treed level's shape as a graph — decision 0092's report, for the edges: what the cut will
/// climb, and on a `dag` layer how far it is from a tree (`dag-hierarchies.md` §3). Rung 3's MeSH
/// layer has 30% of its descriptors under more than one parent, and an operator reading a budget's
/// behaviour there needs that number beside the layer, not in a probe.
///
/// It decides nothing. Reported for every layer whose kind carries edges — a tree's line reads
/// `0 under more than one parent`, which is the cheap way of saying it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyShape {
    pub layer: String,
    pub level: u32,
    pub kind: String,
    pub artifacts: u64,
    /// Parent edges, each once.
    pub edges: u64,
    /// Artifacts naming no parent.
    pub roots: u64,
    /// Artifacts naming more than one parent — nonzero only on a `dag` layer.
    pub multi_parent: u64,
    /// The most parents any artifact names.
    pub max_parents: u64,
}

/// What a build's layer pass produced, for the manifest and for the digest map.
pub struct PublishedLayers {
    /// For a layer scoped to a group, which view's artifact set each published artifact belongs
    /// to: layer → `(level, ordinal)` → the view's key (`views.md` §3.5). Empty for every
    /// unscoped layer, whose one set is drawn on every view it names.
    pub artifact_views: BTreeMap<String, BTreeMap<(u32, u32), String>>,
    pub layers: Vec<RegisteredLayer>,
    /// Edges whose child escapes its parent's membership — reported, never acted on.
    pub containment_violations: Vec<ContainmentViolation>,
    /// Per-parent coverage: how much of each split its children hold between them.
    pub split_coverage: Vec<SplitCoverage>,
    /// Per treed level, its edges as a graph — reported beside the layer and in `containment.json`.
    pub hierarchy_shapes: Vec<HierarchyShape>,
    /// One past the lowest entity the layers and their artifacts claimed. **The mark that must
    /// reach the manifest**: the WAL carries the same one in its records and rotation reclaims
    /// those, so a mark that lived only there is lost at the first rotation and the next
    /// registration is handed ids a live layer already holds (decision 0074).
    pub low_water: u64,
    pub membership_extents: Vec<MembershipExtent>,
    /// Every `(layer, level)`'s artifact-write counter as this build leaves it.
    ///
    /// **Emitted rather than left empty, and the difference is not cosmetic.** A level absent from
    /// the list is a level whose version is *unknown* to the reader that opens the bundle, which
    /// is not the same as zero (`manifest::LevelVersion`) — and a build that published artifacts
    /// and said nothing about their versions would make the first restart's coordinates
    /// unrelatable to the ones the build's own store held.
    ///
    /// **The build now writes every derived structure a fold does** — see `crate::artifact_pass`,
    /// which runs after the segment write and fills the three extent lists below. It composes
    /// through `tessera_store::membership`, beside the formats, rather than through the engine: a
    /// build-side edge on `tessera-engine` is what an earlier revision of this comment ruled out,
    /// and moving the writer down to the format's own crate is what made the edge unnecessary
    /// rather than merely avoided.
    pub level_versions: Vec<tessera_store::manifest::LevelVersion>,
    pub artifact_record_extents: Vec<RecordExtent>,
    /// Every file written here, for `MANIFEST.files` — an undigested file is one a torn write
    /// cannot be attributed to.
    pub paths: Vec<PathBuf>,
    /// Member rows that named no artifact, per source — carried out of the plan so the build's own
    /// report can state the number rather than leaving it on stderr alone.
    pub unclustered: Vec<UnclusteredRows>,
    /// Artifacts each layer's member keys **created**, per layer, carried out for the same reason.
    pub minted: BTreeMap<String, u64>,
    /// **The store this pass published into, carried out for the post-bundle artifact pass**
    /// (`crate::artifact_pass`).
    ///
    /// The pass has to observe where each membership *landed in row space*, and row space does not
    /// exist yet at this stage — the tiler sort is two stages away. So the records travel to the end
    /// of the build rather than being rebuilt from the extents this just wrote, which would decode
    /// every key, content and attachment a second time to reach a structure already in hand.
    ///
    /// It is carried across the build's residency peak, and **what it carries there is a mapping**
    /// (2026-09-02). Each record's membership is read back through the packed extent
    /// `write_membership_extents` has just written and fsynced, so what travels these four stages
    /// is a Roaring container's descriptor per container and page cache for the members
    /// themselves. Held as heap bitmaps it was +1.2 GB of anonymous memory at the 10⁷ MedCPT
    /// sample with 471,778,374 closed MeSH member rows, and ~47 GB extrapolated at 10⁸
    /// (`probes/2026-09-02-mapped-memberships/README.md`).
    pub store: ArtifactStore,
    /// The derived structures the post-bundle pass wrote, for `SEGMENTS-0.json`. Empty until it
    /// runs.
    pub tile_index_extents: Vec<tessera_store::manifest::TileIndexExtent>,
    pub row_column_extents: Vec<tessera_store::manifest::RowColumnExtent>,
    pub containment_extents: Vec<tessera_store::manifest::ContainmentExtent>,
    pub shape_rows_extents: Vec<tessera_store::manifest::ShapeRowsExtent>,
    pub shape_held_extents: Vec<tessera_store::manifest::ShapeHeldExtent>,
}

impl Default for PublishedLayers {
    fn default() -> Self {
        PublishedLayers {
            layers: Vec::new(),
            artifact_views: BTreeMap::new(),
            containment_violations: Vec::new(),
            split_coverage: Vec::new(),
            hierarchy_shapes: Vec::new(),
            low_water: tessera_types::layer::ROWLESS_CEILING,
            membership_extents: Vec::new(),
            level_versions: Vec::new(),
            artifact_record_extents: Vec::new(),
            paths: Vec::new(),
            unclustered: Vec::new(),
            minted: BTreeMap::new(),
            store: ArtifactStore::new(),
            tile_index_extents: Vec::new(),
            row_column_extents: Vec::new(),
            shape_rows_extents: Vec::new(),
            shape_held_extents: Vec::new(),
            containment_extents: Vec::new(),
        }
    }
}

/// Take the config's layer declarations and read each layer's own artifacts against them.
///
/// **One source per layer, so no row names the layer it belongs to.** The layer is the input's
/// own, which is what retires the discriminator column: there is no second layer's rows in the
/// file to tell apart, no filter to configure, and no way for a layer to ingest another's rows
/// (`annotation-write-cycle.md` §6.1).
// The eighth argument is which layers are scoped, and it belongs beside the declarations it
// qualifies: a struct around the seven would be a second spelling of `BuildArgs`' layer half.
#[allow(clippy::too_many_arguments)]
pub fn read(
    declarations: &[LayerDeclaration],
    inputs: &[LayerSources],
    // Which layers are scoped to a group, by name (`views.md` §3.5).
    scoped: &BTreeMap<String, crate::ScopedLayer>,
    // **Every view this build materialises, each with its own frame** (decision 0111): a shape
    // layer is canonicalised against the frame of each view it is drawn in, and a layer spanning
    // views whose frames differ therefore stores a different canonical form under each name.
    frames: &[tessera_store::derived::ViewFrame],
    max_shape_vertices: u64,
    scratch: &Path,
    memory_budget: u64,
) -> Result<LayerPlan> {
    let mut plan = LayerPlan {
        bodies: Vec::new(),
        addresses: Vec::new(),
        declarations: declarations.to_vec(),
        shape_reports: Vec::new(),
        artifacts: BTreeMap::new(),
        unclustered: Vec::new(),
        minted: BTreeMap::new(),
        members: MemberSpill::new(scratch, memory_budget),
        memory_budget,
    };
    for input in inputs {
        // An artifact source names artifacts *in a layer*, and a layer this build does not
        // register is a name the manifest cannot carry. The config produces these parallel to the
        // declarations; a caller assembling them by hand gets the refusal instead of a silently
        // unpublished file.
        let Some(declaration) = declarations.iter().find(|d| d.name == input.name) else {
            return Err(BuildError::Invalid(format!(
                "artifacts are bound for layer '{}', which this build declares no `[[layer]]` \
                 block for",
                input.name
            )));
        };
        let enumerated =
            declaration.membership == tessera_types::layer::MembershipSource::Enumerated;
        // **A shape layer's rows are canonicalised as they are read** — against the frame the
        // points are quantised in and for every view the layer is drawn in — and what that did is
        // reported beside the layer, never refused (`crate::shapes`).
        // **The table's `default_space` is the layer's, not the membership shape's** (§4.3): it is
        // the fallback for every geometry the rows declare, so the authored shape content below
        // resolves against the same value the membership shape does.
        let default_space = match &input.artifacts {
            Some(ArtifactSource::File { default_space, .. }) => *default_space,
            _ => tessera_store::derived::ShapeSpace::View,
        };
        // **This layer's own views, each with its own frame** — never the build's first, which a
        // layer need not be drawn on at all. A view the layer names and this build does not
        // materialise is absent here, and the layer is canonicalised for the views that exist.
        let layer_frames: Vec<tessera_store::derived::ViewFrame> = declaration
            .views
            .iter()
            .filter_map(|name| frames.iter().find(|f| &f.view == name))
            .cloned()
            .collect();
        // Decision 0111's layer-level rule, at the declaration and before a row is read: a mix of
        // projected and unprojected row spaces is a refusal naming the layer and the views.
        if shape_declared(declaration).is_some() {
            tessera_store::derived::check_shape_span(
                &layer_frames,
                tessera_store::derived::ShapeSpace::Wgs84,
            )
            .map_err(|e| BuildError::Invalid(format!("layer '{}': {e}", input.name)))?;
        }
        let mut shapes = shape_declared(declaration).map(|kind| {
            ShapeReader::new(
                &input.name,
                kind,
                ShapeContext {
                    views: layer_frames.clone(),
                    max_vertices: max_shape_vertices,
                },
                default_space,
            )
        });
        match &input.artifacts {
            Some(ArtifactSource::File { path, fields, .. }) => read_artifacts(
                &input.name,
                path,
                fields,
                enumerated,
                scoped.get(&input.name),
                &mut plan,
                shapes.as_mut(),
            )?,
            Some(ArtifactSource::Inline(rows)) => {
                // **A scoped layer's artifacts are read from a file**, because the view each
                // belongs to is a column of it (`views.md` §3.5). The inline spelling has no such
                // column, and taking every row for every view would draw one quarter's clusters on
                // all four.
                if let Some(scope) = scoped.get(&input.name) {
                    return Err(BuildError::Invalid(format!(
                        "layer '{}': `scope = {{ group = \"{}\" }}` with the artifacts written \
                         inline. A scoped layer's rows say which view each artifact belongs to, \
                         under `fields.view`, and an inline row carries no such column (views \
                         §3.5) — write the artifacts to a file, or drop the scope for one set \
                         drawn on every view named",
                        input.name, scope.group
                    )));
                }
                plan_inline(&input.name, rows, &mut plan, shapes.as_mut())?
            }
            // **Which artifacts exist is the layer's own artifact source's to say** — while the
            // layer's value set is closed. Without one a member source would be both the roster and
            // the population, and a mistyped key would publish an artifact rather than fail to find
            // one. An **open** layer asks for exactly that: a cluster exists because points say it
            // does, and a bare clustering declares no artifacts at all
            // (`artifacts-from-points.md` §3).
            None => {
                if let Some(members) = &input.members {
                    if declaration.value_set == ValueSet::Closed {
                        return Err(BuildError::Invalid(format!(
                            "{}: layer '{}' binds members with no artifacts of its own, which is \
                             what declares the artifacts they belong to; a key with no artifact \
                             behind it must be a refusal rather than a new artifact. Declare \
                             `value_set = \"open\"` on the layer to have every key its points \
                             name be an artifact",
                            members.path.display(),
                            input.name
                        )));
                    }
                }
            }
        }
        // The layer's rows are all read, so the reader's report — counts, canonicalisation and
        // the children whose bounds escape their parents (§6.2) — is complete for this layer.
        if let Some(reader) = shapes {
            let parents: Vec<(String, String)> = plan
                .artifacts
                .iter()
                .filter(|((layer, _, _), _)| layer == &input.name)
                .flat_map(|((_, _, key), index)| {
                    plan.bodies[*index]
                        .parent_keys
                        .iter()
                        .map(|parent| (key.clone(), parent.clone()))
                })
                .collect();
            plan.shape_reports.push(reader.finish(parents));
        }
        // **The authored shape content is read as a membership shape is** (`polygon-membership.md`
        // §6.1, ruling (h)): where the declaration's supplied content names a `polygon`, `circle`
        // or `ellipse` kind, that slot of every ranked content — WKT, or the numbers of the kind's
        // row field — goes through the same reader, the same canonicalisation for every view, the
        // same report, the same vertex cap and **the same space** — the table's `default_space`
        // and the row's own `space`, resolved against the view's projection exactly as a
        // membership shape's is (§4.3) — and the slot then carries the canonical bytes in their
        // content spelling for the blob. The serve reads them back into `shape_x`/`shape_y`; the
        // client is never handed the string.
        if let Some((slot, kind)) = declaration.authored_shape() {
            let content_name = declaration.content.supplied[slot].name.clone();
            let mut reader = ShapeReader::new(
                &format!(
                    "{} (authored `{}` content '{content_name}')",
                    input.name,
                    kind.as_str()
                ),
                kind,
                ShapeContext {
                    views: layer_frames.clone(),
                    max_vertices: max_shape_vertices,
                },
                default_space,
            );
            let mine: Vec<(String, usize)> = plan
                .artifacts
                .iter()
                .filter(|((layer, _, _), _)| layer == &input.name)
                .map(|((_, _, key), index)| (key.clone(), *index))
                .collect();
            for (key, index) in mine {
                let space = plan.bodies[index].space.clone();
                for content in &mut plan.bodies[index].contents {
                    let Some(text) = content.values.get_mut(slot) else {
                        // Short of a value: refused where every content is checked for width.
                        continue;
                    };
                    let shape = authored_shape_input(kind, text).map_err(|e| {
                        BuildError::Invalid(format!(
                            "layer '{}': artifact {key}: the authored `{}` content '{content_name}': {e}",
                            input.name,
                            kind.as_str()
                        ))
                    })?;
                    let Some(canonical) = reader.row(&key, Some(shape), space.as_deref())? else {
                        return Err(BuildError::Invalid(format!(
                            "layer '{}': artifact {key}: the authored `{}` content '{content_name}' \
                             canonicalised to no view",
                            input.name,
                            kind.as_str()
                        )));
                    };
                    *text = canonical.content_text();
                }
            }
            plan.shape_reports.push(reader.finish(Vec::new()));
        }
        if let Some(members) = &input.members {
            let before = plan.artifacts.len();
            let (rows, read) = read_members(
                &input.name,
                &members.path,
                &members.fields,
                declaration,
                &mut plan,
            )?;
            let minted = (plan.artifacts.len() - before) as u64;
            if minted > 0 {
                // Printed on §7's posture, beside the unclustered count and for the same reason: a
                // mistyped key under an open value set creates an artifact instead of refusing, and
                // what tells that apart from a clustering the artifacts source simply does not
                // enumerate is the number.
                eprintln!(
                    "layer '{}': {minted} artifact(s) created by keys in {} that no artifacts \
                     source declares",
                    input.name,
                    members.path.display()
                );
                *plan.minted.entry(input.name.clone()).or_default() += minted;
            }
            if rows > 0 {
                // Printed here, where the source and its layer are both in hand, on §7's posture:
                // the operator is present, the numbers are what tell a noisy clustering from a
                // wrong column, and neither is a reason to block a build. **Against the rows read**,
                // because that is the denominator that separates the two: a quarter is a condensed
                // tree's noise and all of them is the wrong column.
                eprintln!(
                    "layer '{}': {rows} of {read} rows in {} are in no artifact (a null key, or -1)",
                    input.name,
                    members.path.display()
                );
                plan.unclustered.push(UnclusteredRows {
                    layer: input.name.clone(),
                    source: members.path.clone(),
                    rows,
                });
            }
        }
    }
    Ok(plan)
}

/// One row per artifact: its scalars, its ranked `contents`, its membership and the edges it hangs
/// from.
///
/// **One row, so there is nothing to agree with.** The earlier grain was one row per
/// `(artifact, rank)`, which repeated the key, the parent and the attachment on every row of one
/// artifact so that a single column could differ — and the build had to check the copies matched,
/// including the case where a later row named none. A ranked list in one cell removes the
/// disagreement rather than detecting it. What one row per artifact *does* admit is the same
/// artifact written twice, which is refused below: two rows for one key are two artifacts as far
/// as the file is concerned, and taking either would be taking the file's row order for an answer.
#[allow(clippy::too_many_arguments)]
fn read_artifacts(
    layer: &str,
    path: &Path,
    fields: &Fields,
    enumerated: bool,
    scoped: Option<&crate::ScopedLayer>,
    plan: &mut LayerPlan,
    mut shapes: Option<&mut ShapeReader>,
) -> Result<()> {
    for batch in batches(path)? {
        let batch = batch?;
        let key = key_column(path, &batch, fields, "key")?;
        // **Which view each artifact belongs to**, on a layer scoped to a group (`views.md`
        // §3.5). Required where the scope is declared: a row that names no view belongs to no
        // artifact set, and every view's own selection would pass it over.
        let view: Option<&StringArray> = match scoped {
            None => None,
            Some(scope) => {
                let array = batch.column_by_name(&scope.column).ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "{}: layer '{layer}' is scoped to group '{}' and reads the view each \
                         artifact belongs to from a column named '{}', which this file does not \
                         carry. Its columns are: {}",
                        path.display(),
                        scope.group,
                        scope.column,
                        column_names(&batch)
                    ))
                })?;
                Some(typed(path, array, &scope.column)?)
            }
        };
        let shape_columns = match shapes.as_ref() {
            Some(reader) => Some(ShapeColumns::open(path, &batch, fields, reader.kind())?),
            None => None,
        };
        // Read whether or not the layer declares a membership shape: the same column is the space
        // of the row's authored shape content, which the second pass below reads (§6.1).
        let spaces = space_column(path, &batch, fields)?;
        let level = optional_u32(path, &batch, LEVEL)?;
        let contents = optional_ranked_values(path, &batch, fields, "contents")?;
        let members = optional_u64_list(path, &batch, fields, "members")?;
        let excluding = optional_u64_list(path, &batch, fields, "excluding")?;
        let target_layer = optional_utf8(path, &batch, fields, "attached_layer")?;
        let target_level = optional_u32(path, &batch, ATTACHED_LEVEL)?;
        let target_key = optional_utf8(path, &batch, fields, "attached_key")?;
        let parent = parent_column(path, &batch, fields)?;

        // **A stored membership on a layer whose members are computed is a refusal**, not a
        // column read anyway: `membership` decides what a write invalidates, and a spatial or
        // predicate layer's members come from the shape or the predicate at request time — so a
        // set stored beside it is one nothing would read, or worse, one that quietly did.
        if !enumerated && (members.is_some() || excluding.is_some()) {
            return Err(BuildError::Invalid(format!(
                "{}: layer '{layer}' carries a stored membership column, and its `membership` is \
                 not `enumerated` — its members are computed from a shape or a predicate, so a \
                 stored set here is one nothing declared",
                path.display()
            )));
        }
        // **A membership is included or excluded, never both.** The config refuses a `fields` map
        // naming both; this is the file carrying both columns under their own names, which no map
        // has to mention.
        if members.is_some() && excluding.is_some() {
            return Err(BuildError::Invalid(format!(
                "{}: layer '{layer}' carries both a '{}' and an '{}' column. They are two \
                 spellings of one membership — the entities in it, or the entities it leaves out — \
                 so a row carrying each has two memberships, and every masked count divides by one \
                 of them",
                path.display(),
                fields.of("members"),
                fields.of("excluding"),
            )));
        }

        for row in 0..batch.num_rows() {
            let address = address(path, layer, &level, &key, row)?;
            if plan.artifacts.contains_key(&address) {
                return Err(BuildError::Invalid(format!(
                    "{}: artifact {} is declared on more than one row. One row is one artifact, so \
                     a second row for a key is a second artifact under one name — which of the two \
                     was published would be the file's row order rather than anything the caller \
                     wrote",
                    path.display(),
                    address.2
                )));
            }

            let attachment = match (
                target_layer.as_ref().and_then(|c| value_at(c, row)),
                target_key.as_ref().and_then(|c| value_at(c, row)),
            ) {
                (Some(layer), Some(key)) => Some(IncomingAttachment {
                    layer,
                    level: target_level.as_ref().map_or(0, |c| number_at(c, row)),
                    key,
                }),
                (None, None) => None,
                _ => {
                    return Err(BuildError::Invalid(format!(
                        "{}: artifact {} names half an attachment — an edge needs both \
                         attached_layer and attached_key",
                        path.display(),
                        address.2
                    )))
                }
            };

            // **The lineage is read upward only.** A `children_keys` column beside `parent` was
            // read, checked for cross-row agreement and never walked: containment, coverage and
            // cycle detection all derive children by inverting the parent edges. Two spellings of
            // one edge is one more place for them to disagree, so the column is gone rather than
            // carried.
            let membership = match (&members, &excluding) {
                (Some(column), _) => {
                    PlannedMembership::Included(u64s_at(path, column, row, &address.2)?)
                }
                (_, Some(column)) => {
                    PlannedMembership::Excluded(u64s_at(path, column, row, &address.2)?)
                }
                (None, None) => PlannedMembership::default(),
            };
            let view_key = match (scoped, view) {
                (Some(scope), Some(column)) => {
                    let named = value_at(column, row).ok_or_else(|| {
                        BuildError::Invalid(format!(
                            "{}: artifact {} carries no '{}', and this layer's artifacts are a \
                             different set per view of '{}' (views §3.5) — a row naming no view \
                             is in no artifact set",
                            path.display(),
                            address.2,
                            scope.column,
                            scope.group
                        ))
                    })?;
                    if scope.keys.binary_search(&named).is_err() {
                        return Err(BuildError::Invalid(format!(
                            "{}: artifact {} names view '{named}', which group '{}' has no such \
                             key for. Its keys are: {}. An artifact belongs to one view and its \
                             keys are unique per (layer, view), so a key nobody declared is a \
                             refusal rather than an artifact drawn nowhere (views §3.5)",
                            path.display(),
                            address.2,
                            scope.group,
                            scope.keys.join(", ")
                        )));
                    }
                    Some(named)
                }
                _ => None,
            };
            let index = plan.intern(address.clone());
            plan.bodies[index] = PlannedArtifact {
                view_key,
                membership,
                contents: match contents.as_ref() {
                    None => Vec::new(),
                    Some(column) => ranked_at(path, column, row, &address.2)?,
                },
                attached_to: attachment,
                parent_keys: parents_at(path, parent.as_ref(), row)?,
                shape: match (shapes.as_deref_mut(), shape_columns.as_ref()) {
                    (Some(reader), Some(columns)) => reader.row(
                        &address.2,
                        columns.at(path, row, &address.2)?,
                        space_at(spaces, row),
                    )?,
                    _ => None,
                },
                space: space_at(spaces, row).map(str::to_string),
            };
        }
    }
    Ok(())
}

/// The same artifacts, written out in the declaration itself rather than read from a file.
///
/// **A spelling, not a second kind of layer.** It produces exactly what [`read_artifacts`] does
/// from the same rows, which is what makes an authored layer and a generated one byte-identical in
/// the bundle — the inline route exists so a handful of curated sets need no Parquet file
/// (`annotation-write-cycle.md` §6.1).
fn plan_inline(
    layer: &str,
    rows: &[InlineArtifact],
    plan: &mut LayerPlan,
    mut shapes: Option<&mut ShapeReader>,
) -> Result<()> {
    for row in rows {
        let address = (layer.to_string(), row.level, row.key.clone());
        if plan.artifacts.contains_key(&address) {
            return Err(BuildError::Invalid(format!(
                "layer '{layer}': artifact {} is written twice in the declaration. One entry is \
                 one artifact, so a second is a second artifact under one name",
                row.key
            )));
        }
        let attached_to = match (&row.attached_layer, &row.attached_key) {
            (Some(layer), Some(key)) => Some(IncomingAttachment {
                layer: layer.clone(),
                level: row.attached_level,
                key: key.clone(),
            }),
            (None, None) => None,
            _ => {
                return Err(BuildError::Invalid(format!(
                    "layer '{layer}': artifact {} names half an attachment — an edge needs both \
                     attached_layer and attached_key",
                    row.key
                )))
            }
        };
        let index = plan.intern(address);
        plan.bodies[index] = PlannedArtifact {
            // An inline artifact is on an unscoped layer: the scoped spelling is refused where
            // the source is chosen, having no column to name a view with.
            view_key: None,
            membership: match (&row.members, &row.excluding) {
                // Both is refused at parse, where the declaration can name the artifact.
                (Some(members), _) => PlannedMembership::Included(members.clone()),
                (_, Some(excluding)) => PlannedMembership::Excluded(excluding.clone()),
                (None, None) => PlannedMembership::default(),
            },
            contents: row
                .contents
                .iter()
                .map(|values| PlannedContent {
                    values: values.clone(),
                    generated_from: Vec::new(),
                })
                .collect(),
            attached_to,
            parent_keys: {
                let mut keys = row.parent.clone();
                dedup_keys(&mut keys);
                keys
            },
            shape: match shapes.as_deref_mut() {
                Some(reader) => {
                    let input = inline_shape(row, reader.kind())
                        .map_err(|e| BuildError::Invalid(format!("layer '{layer}': {e}")))?;
                    reader.row(&row.key, input, row.space.as_deref())?
                }
                None => {
                    if row.bbox.is_some()
                        || row.circle.is_some()
                        || row.ellipse.is_some()
                        || row.wkt.is_some()
                    {
                        return Err(BuildError::Invalid(format!(
                            "layer '{layer}': artifact {} carries a shape, and the layer \
                                 declares no `[layer.shape]`. Its members come from the stored \
                                 set its membership names, so a shape beside them is a region \
                                 nothing evaluates",
                            row.key
                        )));
                    }
                    None
                }
            },
            space: row.space.clone(),
        };
    }
    Ok(())
}

/// One row per `(artifact, entity)`: the memberships, and the generating sets beside them.
///
/// A null `rank` is the artifact's **membership**; `rank = k` is `contents[k]`'s
/// generating set — the documents a viewer must be able to see *entirely* before that description
/// is served to them. It is a source of its own rather than a cell on the artifact row because a
/// condensed tree's root holds the whole corpus, which one cell can neither stream nor be
/// materialised by a producer.
///
/// **A point table with a cluster column is this shape already**, which is why membership from a
/// clusterer needed no surface of its own (`artifacts-from-points.md` §2): `fields` names the
/// cluster column as `key` and the id column as `entity`, and the file the build reads its geometry
/// from is also the file it reads its memberships from.
///
/// **A list key column is one row per `(artifact, entity)` as well** — several of them
/// (`artifacts-from-points.md` §4). A hierarchical clusterer emits a list per point, and what the
/// list means is the hierarchy kind the layer already declares: one entry per level for `stacked`
/// and `tiered`, a lineage for `nested`, and a set in no order for `flat` and `dag`. The entries
/// name the artifacts the point belongs to, exactly as a scalar names the one, and `tiered` and
/// `nested` read their **edges** from the adjacency the list itself carries. A `dag` layer's edges
/// are spelled on its artifact rows' `parent` list and never here (decision 0125).
///
/// Returns the rows that named no artifact, and the rows read — the numerator and the denominator
/// the caller prints.
fn read_members(
    layer: &str,
    path: &Path,
    fields: &Fields,
    declaration: &LayerDeclaration,
    plan: &mut LayerPlan,
) -> Result<(u64, u64)> {
    let value_set = declaration.value_set;
    let (mut unclustered, mut read) = (0u64, 0u64);
    // Built on the first integer batch and not before: a text-keyed layer never pays for it, and a
    // layer of 10⁷ artifacts pays once rather than per point.
    let mut roster: Option<KeyRoster> = None;
    // The edges a list column declared, child address → parents. **One entry per child, not one
    // per row**: a cluster of a hundred thousand points states its parent a hundred thousand times,
    // and the second statement onward is a comparison rather than an insertion. Applied once the
    // whole source has been read, so a conflict is found wherever in the file it sits. Only a
    // `nested` or `tiered` list declares edges; a `dag` list is memberships and never reaches
    // this map (`ListMeaning`, decision 0125).
    let mut lineage: Vec<Option<usize>> = Vec::new();
    // Reused across rows rather than allocated per point: one slot per position in the row's list,
    // `None` where the entry named no artifact.
    let mut entries: Vec<Option<usize>> = Vec::new();
    let mut said_level_is_ignored = false;
    // **Decoded ahead, on one other thread, in file order** ([`batches_ahead`]). The row loop below
    // stays serial: it mints into the plan, and the mint order is the file's.
    for batch in batches_ahead(path, READ_AHEAD_BATCHES)? {
        let batch = batch?;
        let key = member_keys(path, &batch, fields, layer, declaration)?;
        let level = optional_u32(path, &batch, LEVEL)?;
        let rank = optional_u32_field(path, &batch, fields, "rank")?;
        let entity = u64s(path, &batch, fields, "entity")?;
        read += batch.num_rows() as u64;

        // **Ignored and said so**, rather than refused or read: the positions in a list are what
        // carry the levels, so a `level` column beside one is a second answer to a question the
        // column has already answered. Reading it would place a point at a level its list did not
        // name; refusing would block a build over an input that discloses nothing and costs a
        // rerun.
        if level.is_some() && matches!(key, MemberKeys::Listed(_)) && !said_level_is_ignored {
            said_level_is_ignored = true;
            eprintln!(
                "layer '{layer}': {} carries a `level` column beside a list key column, whose own \
                 positions are what carry the levels — the column is ignored",
                path.display()
            );
        }

        for row in 0..batch.num_rows() {
            match &key {
                MemberKeys::Scalar(column) => {
                    let at_level = level.map_or(0, |c| number_at(c, row));
                    let Some(member) = resolve_member(
                        layer,
                        at_level,
                        column.read_at(row),
                        value_set,
                        path,
                        plan,
                        &mut roster,
                    )?
                    else {
                        unclustered += 1;
                        continue;
                    };
                    let name = plan.address_of(member).2.clone();
                    // **A null `entity` is a refusal, not entity zero.** Arrow's `value` reads the
                    // values buffer whatever the validity bitmap says, and a Parquet writer leaves
                    // a zero there — so a producer whose join missed a row would publish the
                    // corpus's lowest-numbered document into the cluster, moving its masked count
                    // for every viewer who can see that one document.
                    if entity.is_null(row) {
                        return Err(null_entity(path, &name));
                    }
                    attach_member(
                        plan,
                        member,
                        path,
                        entity.value(row),
                        rank.as_ref().and_then(|c| value_index(c, row)),
                    )?;
                }
                MemberKeys::Listed(listed) => {
                    let Some(positions) = listed.entries(path, layer, row)? else {
                        unclustered += 1;
                        continue;
                    };
                    if entity.is_null(row) {
                        return Err(null_entity(path, &format!("row {row}")));
                    }
                    let source = entity.value(row);
                    let rank = rank.as_ref().and_then(|c| value_index(c, row));

                    entries.clear();
                    for (position, index) in positions.enumerate() {
                        entries.push(resolve_member(
                            layer,
                            listed.meaning.level_of(position),
                            listed.values.read_at(index),
                            value_set,
                            path,
                            plan,
                            &mut roster,
                        )?);
                    }
                    // **A row whose every entry is noise is one row in no artifact**, counted
                    // exactly as a null scalar key is: the denominator the report divides by is
                    // rows, and a list naming nothing is one of them.
                    if entries.iter().all(Option::is_none) {
                        unclustered += 1;
                        continue;
                    }
                    for member in entries.iter().flatten() {
                        attach_member(plan, *member, path, source, rank)?;
                    }
                    if listed.meaning.declares_edges() {
                        record_lineage(&entries, plan, &mut lineage, path)?;
                    }
                }
            }
        }
    }
    apply_lineage(plan, lineage, path)?;
    Ok((unclustered, read))
}

/// One member row's key, resolved against the plan — **minting where the value set is open**, and
/// `None` where the key said the point is in no artifact.
///
/// **The artifacts are the roster, and a key not on them is a refusal** — while the layer's value
/// set is closed. A mistyped key would otherwise publish a phantom artifact carrying the members it
/// stole from a real one: an extra cluster nobody wrote, beside a real cluster whose masked count is
/// quietly short and which may fall below its own criterion and vanish. Neither has an error
/// anywhere to notice.
///
/// **Open lifts exactly that refusal** (`artifacts-from-points.md` §3): the key creates an artifact
/// carrying no title, no parent and no contents — the cluster exists because a point says it does,
/// and the artifacts source, if there is one, is enrichment. A list column mints from the same call,
/// so an interior parent no artifact declares is minted on the same rule as a leaf.
fn resolve_member(
    layer: &str,
    level: u32,
    read: KeyRead<'_>,
    value_set: ValueSet,
    path: &Path,
    plan: &mut LayerPlan,
    roster: &mut Option<KeyRoster>,
) -> Result<Option<usize>> {
    Ok(match read {
        KeyRead::Unclustered => None,
        KeyRead::Named(name) => {
            // **Interned exactly as an integer key is**, and for the same reason: the address is
            // built once per artifact rather than once per point. Building it here allocated the
            // layer name and the key on every member entry and then probed a `BTreeMap` whose
            // comparison walks both — three times over, counting `attach_member` and the lineage.
            let roster = roster.get_or_insert_with(|| KeyRoster::of_layer(layer, plan));
            match roster.text(level, name) {
                Some(index) => Some(index),
                None => {
                    let address = (layer.to_string(), level, name.to_string());
                    if value_set == ValueSet::Closed {
                        return Err(undeclared_key(path, &address));
                    }
                    let index = plan.intern(address.clone());
                    Some(roster.insert_text(level, name, address, index))
                }
            }
        }
        KeyRead::Numbered(value) => {
            let roster = roster.get_or_insert_with(|| KeyRoster::of_layer(layer, plan));
            match roster.get(level, value) {
                Some(index) => Some(index),
                None => {
                    // The one decimal string an integer key ever costs: once per artifact minted,
                    // never once per point. The spelling is the one
                    // `tessera_types::layer::integer_key` states for the wire — `3` and "3" name
                    // one artifact — taken here without allocating for the point that matched.
                    let address = (layer.to_string(), level, value.to_string());
                    if value_set == ValueSet::Closed {
                        return Err(undeclared_key(path, &address));
                    }
                    // **Minted into the plan, never over it.** Absent from the roster is not absent
                    // from the plan — the roster is built once and indexes only the keys that spell
                    // an integer exactly — so an insert here would replace an artifact that already
                    // holds members with an empty one.
                    let index = plan.intern(address.clone());
                    Some(roster.insert(level, value, address, index))
                }
            }
        }
    })
}

/// Put one source entity into an artifact — its membership, or the generating set of one rank.
fn attach_member(
    plan: &mut LayerPlan,
    index: usize,
    path: &Path,
    source: u64,
    rank: Option<u32>,
) -> Result<()> {
    // **One index, not a probe.** The key was resolved to this once when the artifact was first
    // met; every member entry after that writes straight through it.
    //
    // **The key is read only where the refusal fires.** Naming it up front cloned a heap `String`
    // per member entry and dropped it unread — 5×10⁸ times at the Overture rung. The plan is taken
    // apart by field so the refusal can still name it while the body is mutably borrowed.
    let LayerPlan {
        bodies,
        addresses,
        members,
        ..
    } = plan;
    let entry = &mut bodies[index];
    match rank {
        // **Straight to the spill, never into the plan.** This is the one call the whole member
        // path funnels through, so it is the one place the corpus's memberships could accumulate.
        None => match &entry.membership {
            PlannedMembership::Rows => members.push(index, source)?,
            // **Two answers to what an artifact's members are.** The config refuses the two
            // declarations together; this is the same rule for a caller who bound the sources by
            // hand, and it is fail-closed either way — taking one would make a masked count, and
            // the criterion that divides by it, depend on which source the reader happened to read
            // first.
            _ => {
                return Err(BuildError::Invalid(format!(
                    "{}: {} has a membership on its own row and another in this member source. \
                     They are two shapes of one thing, so which one a masked count divides by \
                     would be the order the sources were read",
                    path.display(),
                    addresses[index].2
                )))
            }
        },
        // ⊘ **A generating set is still held in the plan.** It is one list per ranked content per
        // artifact rather than one per member row, so it is not the term this spill exists to
        // bound — but a member source carrying a `rank` column over a corpus-sized generating set
        // would accumulate here exactly as memberships used to.
        Some(rank) => content_at_rank(entry, rank).generated_from.push(source),
    }
    Ok(())
}

fn null_entity(path: &Path, what: &str) -> BuildError {
    BuildError::Invalid(format!(
        "{}: {what} has a null entity; a null is not entity zero, and publishing it as one puts a \
         document nobody named into the artifact",
        path.display()
    ))
}

/// The edges one row's list declares, folded into what each child's parent is.
///
/// The adjacency itself is [`parent_edges`]'s — the wire reads the same rule off the same function
/// — and what is added here is the conflict: **one entry per child, not one per row**, so a cluster
/// of a hundred thousand points states its parent a hundred thousand times and the second statement
/// onward is a comparison rather than an insertion.
///
/// **One slot per artifact rather than a keyed map.** A child names one parent or the build is
/// refused, so the record is one index and the artifact's own position in the plan addresses it.
/// A three-level tiered layer states an edge twice per member row — 1.94×10⁸ edges over the GBIF
/// ladder corpus — and a `BTreeMap` of that size answers each of them by walking about six nodes
/// to reach a slot an index reaches in one (`probes/2026-09-09-layers-cost/`).
fn record_lineage(
    entries: &[Option<usize>],
    plan: &LayerPlan,
    lineage: &mut Vec<Option<usize>>,
    path: &Path,
) -> Result<()> {
    for (parent, child) in parent_edges(entries) {
        if *child >= lineage.len() {
            lineage.resize(plan.bodies.len().max(*child + 1), None);
        }
        match lineage[*child] {
            Some(named) if named == *parent => continue,
            Some(named) => {
                return Err(two_parents(
                    path,
                    plan.address_of(*child),
                    &plan.address_of(named).2,
                    &plan.address_of(*parent).2,
                ))
            }
            None => lineage[*child] = Some(*parent),
        }
    }
    Ok(())
}

/// Hang every child the column named under the parents it named.
///
/// **A parent already on the artifact row must be the same one**: a `parent` column and a lineage
/// column are two spellings of one edge, and an artifact holding a different parent in each is the
/// same conflict as two points disagreeing. A duplicate edge is one edge whichever spelling stated
/// it.
fn apply_lineage(
    plan: &mut LayerPlan,
    lineage: Vec<Option<usize>>,
    path: &Path,
) -> Result<()> {
    // **Applied in address order, not arena order.** The conflict below is a refusal, and which of
    // several a corpus carries is reported must not depend on the order keys happened to be met —
    // it is the order they sort in, which is what it has always been. One sort of at most one entry
    // per child, against one probe per member row.
    let mut in_order: Vec<(usize, usize)> = lineage
        .into_iter()
        .enumerate()
        .filter_map(|(child, parent)| parent.map(|parent| (child, parent)))
        .collect();
    in_order.sort_by(|a, b| plan.address_of(a.0).cmp(plan.address_of(b.0)));
    for (child, parent) in in_order {
        let address = plan.address_of(child).clone();
        let parent_key = plan.address_of(parent).2.clone();
        let artifact = &mut plan.bodies[child];
        if artifact.parent_keys.contains(&parent_key) {
            continue;
        }
        if let Some(declared) = artifact.parent_keys.first() {
            return Err(two_parents(path, &address, declared, &parent_key));
        }
        artifact.parent_keys.push(parent_key);
    }
    Ok(())
}

/// Each key once, in order of first appearance: a duplicate edge is one edge
/// (`dag-hierarchies.md` §4).
fn dedup_keys(keys: &mut Vec<String>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    keys.retain(|key| seen.insert(key.clone()));
}

/// **A child naming two different parents is refused** (`artifacts-from-points.md` §4). The data is
/// not the tree the layer declared: there is no correct output, and choosing a parent would publish
/// a hierarchy the caller did not write. A `dag` layer never reaches this: its list column is
/// memberships and declares no edges, and its several parents are spelled on the artifact row
/// (`dag-hierarchies.md` §4, decision 0125).
fn two_parents(path: &Path, child: &Address, first: &str, second: &str) -> BuildError {
    BuildError::Invalid(format!(
        "{}: {} in level {} of {} is named as a child of both {first} and {second}. A list key \
         column declares the edges, so two rows naming different parents for one artifact are two \
         hierarchies — and which of them was published would be the file's row order rather than \
         anything the caller wrote. A child under several parents is a `dag` layer, whose edges \
         are spelled on the artifact row's `parent` list",
        path.display(),
        child.2,
        child.1,
        child.0
    ))
}

/// The content at `rank`, growing the ranking to reach it.
///
/// **Dense, and a gap is refused.** A ranking is the caller's ordering and the service takes no
/// opinion on it (decision 0078), so a missing rank 1 under a present rank 2 would either
/// renumber the caller's ranking or publish an empty description; both are answers nobody wrote.
fn content_at_rank(artifact: &mut PlannedArtifact, index: u32) -> &mut PlannedContent {
    let index = index as usize;
    if index >= artifact.contents.len() {
        // Grown rather than refused here: rows arrive in file order, so a rank 2 seen before
        // a rank 1 is ordinary. A gap that is still a gap when the artifact is published is
        // caught there, over the whole ranking, by the empty-content refusal.
        artifact
            .contents
            .resize_with(index + 1, PlannedContent::default);
    }
    &mut artifact.contents[index]
}

/// Register every declaration, publish every artifact, and write the extents that carry them.
///
/// `resolve` maps a **source** entity id to the entity this build assigned it, and `high_water` is
/// the point region's mark — passed so the allocator refuses rather than letting the two regions
/// meet unnoticed.
#[allow(clippy::too_many_arguments)]
pub fn publish(
    plan: &mut LayerPlan,
    resolve: &(dyn Fn(u64) -> Option<u64> + Sync),
    high_water: u64,
    prefix_dir: &Path,
    partition: &str,
    views: &[String],
    derived: &BTreeMap<String, Vec<String>>,
) -> Result<PublishedLayers> {
    let mut registry = LayerRegistry::new();
    let mut alloc = Allocator::new(high_water);
    let mut store = ArtifactStore::new();

    for declaration in &plan.declarations {
        let name = declaration.name.clone();
        // **A view this build does not write is a refusal, not a layer that waits.** A layer
        // appears only in the views it declares, so a mistyped view name would produce a bundle
        // whose layer is registered, reachable, and serves nothing — indistinguishable, from every
        // client, from a layer whose artifacts all failed their existence criterion.
        if let Some(unknown) = declaration
            .views
            .iter()
            .find(|declared| !views.contains(declared))
        {
            return Err(BuildError::Invalid(format!(
                "layer {name} declares view {unknown}, and this build writes {}. A layer \
                 in a view this build does not write is registered, reachable and empty, which no \
                 client can tell from one whose artifacts were all withheld",
                views.join(", ")
            )));
        }
        let record = registry
            .prepare_create(declaration.clone(), &mut alloc)
            .map_err(|e| BuildError::Invalid(format!("layer {name}: {e}")))?;
        registry.apply(&record);
    }

    verify_dependencies(plan)?;

    // **A predicate layer's artifacts are minted from its own column, before anything else is
    // published** (`design/artifact-serving-at-scale.md` §5.1). The keys arrive already in
    // value-code order, which is what makes an ordinal a function of the *value* rather than of
    // when the build happened to see it — the same determinism rule the key ordering below gives
    // an enumerated layer, read over the vocabulary instead of over a file.
    //
    // ⊘ A predicate layer sits at level 0 and nowhere else: `LayerDeclaration::validate` refuses it
    // levels, because the rule produces one artifact per value at one resolution.
    for declaration in &plan.declarations {
        let Some(keys) = derived.get(&declaration.name) else {
            continue;
        };
        if keys.is_empty() {
            continue;
        }
        let record = registry
            .prepare_derive(&declaration.name, 0, keys, &store, &mut alloc)
            .map_err(|e| BuildError::Invalid(format!("deriving into {}: {e}", declaration.name)))?;
        registry.apply(&record);
        let refused = store.apply(&record, 0);
        if refused > 0 {
            // The membership publication's finding, at the derived layers' own site: what this
            // catches is a bitmap that was already malformed, not a format that lost it.
            return Err(BuildError::Invalid(format!(
                "{refused} derived artifact(s) of {} were not well-formed bitmaps when this build \
                 encoded them",
                declaration.name
            )));
        }
    }

    // **Every membership is materialised here, before anything else looks at one.** A membership
    // declared by exclusion is complemented against the entity space this build assigned, once, so
    // the hierarchy checks below and the store beneath them see the same set an inclusion would
    // have written — and no later stage has a spelling left to learn.
    //
    // **The member runs are merged into per-artifact contiguity first**, which is where every
    // member row's source id meets `resolve` and where the memberships stop being a stream and
    // become a membership. Nothing before this point held more than the spill's own budget of
    // them, and nothing after this point holds more than one artifact's.
    let receipts = plan.members.finish()?;
    let table = merge_member_runs(&receipts, plan, resolve)?;

    // **Resolved in parallel, in place.** Each artifact's resolution reads only its own planned
    // body and the `resolve` closure, which is a lookup into two immutable arrays — so there is no
    // cross-artifact state and nothing to order. The publication order is read off `plan.artifacts`
    // below and is therefore the keys', never the scheduler's, which is what keeps ordinals a
    // function of the artifacts under I9.
    //
    // What is left here is the artifacts' own rows: the contents, the attachment, the lineage, and
    // the two membership spellings a row carries itself. The member sources' rows went through the
    // merge above.
    //
    // **Indexed by body, not keyed by address.** The plan already holds the `(layer, level, key)`
    // order in `artifacts`, so a second `BTreeMap` keyed on the same addresses bought nothing and
    // cost two heap `String`s per artifact — 930,000 allocations at the GeoNames rung.
    let addresses = &plan.addresses;
    let mut resolved: Vec<ResolvedArtifact> = plan
        .bodies
        .par_iter_mut()
        .enumerate()
        .map(|(index, body)| {
            let (layer, level, key) = &addresses[index];
            resolve_artifact(layer, *level, key, body, resolve, high_water)
        })
        .collect::<Result<_>>()?;

    // **The hierarchy checks run before a single entity is allocated**, which is both the cheaper
    // and the more useful order: they read `declarations` and `resolved` and touch neither the
    // registry nor the store, and a structural fault in the edges is worth refusing before the
    // publication that assigns permanent ids rather than after it. It is also the last reader of a
    // membership before the publication takes it — the memberships are the largest thing this stage
    // holds, and nothing after this point needs them whole.
    let (violations, coverage, hierarchy_shapes) =
        verify_hierarchies(&plan.declarations, &plan.artifacts, &resolved, &table)?;

    // Grouped by `(layer, level)`, each level's artifacts in key order — so a level's
    // ordinals, and therefore its entities, are a function of the artifacts and never of the file's
    // row order. The plan's own map is already in that order, so this is a walk rather than a sort.
    let mut batched: BTreeMap<(&str, u32), Vec<(&str, usize)>> = BTreeMap::new();
    for ((layer, level, key), index) in &plan.artifacts {
        batched
            .entry((layer.as_str(), *level))
            .or_default()
            .push((key.as_str(), *index));
    }

    // **Published in declaration order, which is the order that honours `depends_on`.** An
    // attachment resolves against what is already published, so a label layer must follow the layer
    // it attaches into — and iterating the map instead would publish in alphabetical order, making
    // an operator's file work or fail on how their layers happen to sort.
    let mut order: Vec<(&str, u32)> = Vec::with_capacity(batched.len());
    for declaration in &plan.declarations {
        let name = declaration.name.as_str();
        order.extend(batched.keys().filter(|(layer, _)| *layer == name).copied());
    }

    // **A level is published in batches, and each batch's records go into the level's membership
    // pack as soon as they are published.** A level of 3.4×10⁹ entries built whole is 46 GB of
    // Roaring — the bitmaps this builds, the copy `prepare_publish` takes of each, and the store's
    // own copy behind them — against a model term of 4 B an entry
    // (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §5). Encoding a
    // batch at once and vacating its records ([`ArtifactStore::vacate_members`]) bounds all three
    // at one batch.
    //
    // **Ordinals and entities are what they were.** A level's ordinals are dense and allocated
    // from `next_ordinal` in the order the artifacts are handed over, which is the level's key
    // order in every batch; the entity of an ordinal is a function of the level's reservation
    // runs, and splitting the growth across batches appends the same runs in the same order
    // because nothing else allocates between two batches of one level.
    let members_dir = prefix_dir
        .join("partitions")
        .join(partition)
        .join("members");
    std::fs::create_dir_all(&members_dir).map_err(|e| BuildError::io(&members_dir, e))?;
    let entries_per_batch = publication_batch_entries(plan.memory_budget);
    eprintln!(
        "layers: publishing in batches of at most {entries_per_batch} member entr(ies), \
         {PUBLICATION_BYTES_PER_ENTRY} B an entry against a {}th of the {} MiB budget",
        PUBLICATION_BUDGET_SHARE,
        plan.memory_budget >> 20
    );
    // Each level's pack, written here under a name no final one can take and renamed at
    // [`write_membership_extents`], which is where the extent index in a pack's filename is
    // fixed. Renaming rather than naming it here keeps that index the one `pending_ranges`
    // produces, whatever order the declarations put the levels in.
    let mut streamed: BTreeMap<(String, u32), StreamedPack> = BTreeMap::new();

    for address in order {
        let (layer, level) = address;
        let artifacts = batched
            .remove(&address)
            .expect("every address came from the map a statement ago");
        // **The bodies come out of the plan in key order, and the memberships are read and encoded
        // in parallel.** An artifact's bitmap is a function of its own extent of the member table
        // and of nothing else, so there is no state between two of them; the cost is the read, the
        // sort and the Roaring append, which is 6.3 s of `gbif-240p`'s publication
        // (`probes/2026-09-09-layers-cost/README.md`).
        //
        // **`prepare_publish` still sees the level in key order.** An ordinal is identity under I9
        // and it is assigned off the order the level is handed over in, so the parallel pass is
        // indexed and its results are collected in the order `artifacts` holds — build in
        // parallel, publish in order. The refusals are sequenced the same way, so which artifact a
        // malformed extent is reported against is the level's order rather than the scheduler's.
        //
        // **The batch boundaries are fixed before a body is taken**, off the member table's own
        // extents, so the sizing reads the entries an artifact has rather than the bitmap it
        // will become.
        let splits = publication_batches(
            &artifacts,
            &resolved,
            &table,
            entries_per_batch,
            within_level_edges(&plan.declarations, layer),
        );
        let ordinal_lo = store.next_ordinal(layer, level);
        let pack_path = members_dir.join(format!("streaming-{:03}.tsmb", streamed.len()));
        let mut writer = tessera_store::membership::PackWriter::create(
            &pack_path,
            ordinal_lo,
            artifacts.len() as u32,
        )
        .map_err(BuildError::Store)?;
        let mut start = 0usize;
        let batches = splits.len() as u64;
        for end in splits {
            let batch = &artifacts[start..end];
            let bodies: Vec<PublishableBody> = batch
                .iter()
                .map(|(_, index)| resolved[*index].take_body())
                .collect();
            let built: Vec<Result<IncomingArtifact>> = batch
                .par_iter()
                .zip(bodies)
                // One artifact's bytes and one artifact's entities per task, reused across the
                // artifacts in it — what keeps the publication's residency a batch of Roaring
                // bitmaps plus the largest single artifact per thread, rather than a batch of
                // entity vectors beside them.
                .map_init(
                    || (Vec::<u8>::new(), Vec::<u64>::new()),
                    |(scratch, buf), ((key, index), body)| {
                        incoming_artifact(
                            key,
                            body,
                            &resolved[*index].members,
                            *index,
                            &table,
                            scratch,
                            buf,
                        )
                    },
                )
                .collect();
            let mut incoming = Vec::with_capacity(built.len());
            for artifact in built {
                incoming.push(artifact?);
            }
            let record = registry
                .prepare_publish(
                    layer,
                    level,
                    &incoming,
                    &store,
                    &mut alloc,
                    &tessera_lifecycle::no_pending,
                )
                .map_err(|e| BuildError::Invalid(format!("publishing into {layer}: {e}")))?;
            // The record carries its own copy of every membership, so the bitmaps this built are
            // dead the moment `prepare_publish` returns — a batch's worth of them, held to the
            // end of the iteration for nothing.
            drop(incoming);
            registry.apply(&record);
            let refused = store.apply(&record, 0);
            if refused > 0 {
                // **Reached once, and not by a fault in the encoding.** The 5×10⁷ tier of
                // `probes/2026-08-22-artifact-serving-e2e/` stopped here on one membership of
                // `generator/treed`; the bytes carried exactly what the container held, and what
                // the decoder's validation rejected was a container whose array was already out
                // of order when it was serialised. The message says that rather than blaming the
                // format, because an operator told the encoding failed will look at the wrong
                // half.
                //
                // A refusal rather than an assertion because the alternative is a level published
                // with artifacts silently missing, which serves as *absent* with nothing
                // reporting a fault.
                return Err(BuildError::Invalid(format!(
                    "{refused} membership(s) of {layer} were not well-formed bitmaps when this \
                     build encoded them — the bytes decode to nothing, so the level is refused \
                     rather than published with those artifacts absent"
                )));
            }
            // **Encoded now, while this batch is the only one in hand.** The blobs go into the
            // level's pack in ordinal order, which is the order the batches are published in.
            let batch_lo = ordinal_lo + start as u32;
            let batch_len = (end - start) as u32;
            for blob in store.encode_pending(layer, level, batch_lo, batch_len) {
                let blob = blob.ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "{layer} level {level} has no record at an ordinal this publication just \
                         assigned"
                    ))
                })?;
                writer.push(&blob).map_err(BuildError::Store)?;
            }
            // The bytes are in the writer, so the store's own bitmaps have one reader left — the
            // rehousing that replaces them with a view over the finished pack.
            for ordinal in batch_lo..batch_lo + batch_len {
                store.vacate_members(layer, level, ordinal);
            }
            crate::trim_heap();
            start = end;
        }
        writer.finish().map_err(BuildError::Store)?;
        // A level published in batches is one publication, and the level's version counter says
        // so: how the publication was cut is a memory budget's business and never a bundle's.
        store.fold_publication_versions(layer, level, batches);
        streamed.insert(
            (layer.to_string(), level),
            StreamedPack {
                path: pack_path,
                ordinal_lo,
                count: artifacts.len() as u32,
            },
        );
    }

    // **Which view's set each published artifact belongs to**, on a layer scoped to a group
    // (`views.md` §3.5) — **read back off the records**, which carry the view since it became
    // part of the identity (`ingest.md` §1.5): the artifact pass draws an artifact in its own
    // view and in no other, and an ordinal is what it has to say that with.
    let mut artifact_views: BTreeMap<String, BTreeMap<(u32, u32), String>> = BTreeMap::new();
    let scoped: Vec<String> = registry
        .iter()
        .filter(|(_, layer)| layer.declaration.scope.group().is_some())
        .map(|(name, _)| name.to_string())
        .collect();
    for layer in &scoped {
        for (level, ordinal, record) in store.layer(layer) {
            if let Some(view) = &record.view {
                artifact_views
                    .entry(layer.clone())
                    .or_default()
                    .insert((level, ordinal), view.clone());
            }
        }
    }

    let (layers, _tombstones) = registry.snapshot();
    let mut published = PublishedLayers {
        layers,
        artifact_views,
        containment_violations: violations,
        split_coverage: coverage,
        hierarchy_shapes,
        low_water: alloc.low_water(),
        unclustered: plan.unclustered.clone(),
        minted: plan.minted.clone(),
        ..PublishedLayers::default()
    };
    published.level_versions = store
        .level_versions()
        .map(
            |(layer, level, version)| tessera_store::manifest::LevelVersion {
                layer: layer.to_string(),
                level,
                version,
            },
        )
        .collect();
    write_membership_extents(&mut store, prefix_dir, partition, &mut published, &mut streamed)?;
    write_content_extent(&store, prefix_dir, partition, &mut published)?;
    published.store = store;
    Ok(published)
}

/// Merge every member run into one **member table**: each artifact's members contiguous, resolved
/// to the entities this build assigned, and ascending.
///
/// **This is where the member rows meet `resolve`**, and it is the only place they do. The
/// resolution used to run per artifact over a vector the plan held; the vector is the merge's
/// output now, one artifact at a time, so what is resident is the largest single artifact's
/// members and not the corpus's — **once**, the sources being rewritten into their entities rather
/// than read into a second vector.
///
/// ⊘ **The sort is per artifact and it is why the largest one is the bound.** A source id's entity
/// is not a monotone function of it — a build assigns entities in signature-sorted order — so the
/// merge's ascending *sources* come out as unordered *entities*, and the membership has to be in
/// hand to be put in order. The largest artifact at the Overture rung holds 16.3×10⁶ pairs — one
/// division, 5.5% of `members-divisions.parquet`'s 294.1×10⁶ — which is 130 MB for the one
/// artifact that has them, and 130 MB is what the rewrite above stops holding twice.
///
/// ⊘ **The largest *key* in that corpus is not an artifact, and a pair count taken from the
/// column says it is.** `members-taxonomy.parquet`'s key column is a list per row, and 228.8×10⁶
/// of its 441.8×10⁶ entries — 51.8% — are **null**: a point in no artifact at that level, counted
/// as unclustered and never pushed to the spill ([`read_members`]). Flattening the column and
/// counting values reads as one artifact holding half the corpus, and there is no such artifact;
/// that ladder's real contribution is 213.0×10⁶ pairs across 2,097 keys, and the corpus's whole
/// spill is 5.07×10⁸.
fn merge_member_runs(
    receipts: &[spill::SpillReceipt],
    plan: &LayerPlan,
    resolve: &(dyn Fn(u64) -> Option<u64> + Sync),
) -> Result<spill::MemberTable> {
    if receipts.is_empty() {
        return Ok(spill::MemberTable::empty(plan.bodies.len()));
    }
    let receipts = cascade_member_runs(receipts, &plan.members.dir)?;
    let path = plan.members.dir.join("member-table.spill");
    let mut writer = spill::MemberTableWriter::create(&path, plan.bodies.len())?;
    let mut merge = MemberRunMerge::open(&receipts)?;
    let mut pairs = 0u64;
    while merge.next_artifact()? {
        let index = merge.index() as usize;
        let (layer, level, key) = &plan.addresses[index];
        // **Resolved in place, in the merge's own buffer.** A source id and the entity it resolves
        // to are both `u64`, so the artifact's sources *become* its entities; a second vector held
        // the largest single membership of the corpus twice, which at the Overture rung is
        // 16.3×10⁶ pairs and 130 MB apiece.
        let entities = merge.sources_mut();
        for slot in entities.iter_mut() {
            let source = *slot;
            *slot = resolve(source).ok_or_else(|| {
                BuildError::Invalid(format!(
                    "{layer} level {level} artifact {key}: membership names entity {source}, \
                     which this build did not assign — the batch is refused rather than published \
                     without it, a dropped member moving both the count a viewer is shown and the \
                     size a proportional criterion divides by"
                ))
            })?;
        }
        entities.sort_unstable();
        pairs += entities.len() as u64;
        writer.push(index, entities)?;
    }
    // **Every pair that went in came back out.** Each run verifies its own count and anchor as it
    // ends, so what this adds is the one thing no single run can see: that the *set* of runs is
    // whole. A run file lost between the spill and the merge would otherwise be a membership
    // quietly short by however much it held, which is the failure mode the receipts exist for.
    if pairs != plan.members.entries {
        return Err(BuildError::Invalid(format!(
            "the member merge yielded {pairs} member entries where the sources held {} — a run \
             file is missing or was not merged, and a short membership moves every masked count \
             the artifact feeds",
            plan.members.entries
        )));
    }
    drop(merge);
    for receipt in &receipts {
        let _ = std::fs::remove_file(&receipt.path);
    }
    writer.finish()
}

/// Reduce `receipts` to at most [`MEMBER_MERGE_FAN_IN`] runs, deleting each pass's inputs as it
/// goes — so a build's transient disk is the runs at one level of the cascade and not all of them.
///
/// ⊘ **Unreached by anything measured.** At the accumulator's ceiling a run holds 67×10⁶ pairs, and
/// GeoNames' 68.4×10⁶ spilled **two**. It exists because a small `--memory-budget` over a large
/// corpus is the caller's to choose: the budget is a sixteenth of that flag, so it is the flag and
/// not the corpus that decides whether a cascade happens at all.
fn cascade_member_runs(
    receipts: &[spill::SpillReceipt],
    dir: &Path,
) -> Result<Vec<spill::SpillReceipt>> {
    let mut receipts = receipts.to_vec();
    let mut pass = 0usize;
    while receipts.len() > MEMBER_MERGE_FAN_IN {
        let mut merged = Vec::with_capacity(receipts.len().div_ceil(MEMBER_MERGE_FAN_IN));
        for (group, runs) in receipts.chunks(MEMBER_MERGE_FAN_IN).enumerate() {
            let path = dir.join(format!("member-cascade-{pass}-{group:04}.spill"));
            let mut writer = spill::MemberRunWriter::create(&path)?;
            let mut merge = MemberRunMerge::open(runs)?;
            while merge.next_artifact()? {
                // The concatenation of several runs' records for one artifact is not itself
                // ascending, and a run that is not ascending cannot be delta-encoded — so the
                // intermediate is sorted where the final table would have sorted by entity anyway.
                merge.sort_sources();
                writer.push(merge.index(), merge.sources())?;
            }
            merged.push(writer.finish()?);
            drop(merge);
            for run in runs {
                let _ = std::fs::remove_file(&run.path);
            }
        }
        receipts = merged;
        pass += 1;
    }
    Ok(receipts)
}

/// A k-way merge over open run cursors, yielding each artifact once with every source id any run
/// holds for it.
///
/// The heap holds `(head artifact, run)` pairs — an artifact index is four bytes, so unlike the
/// text index's merge there is nothing to be gained by reaching into the cursors to compare. Ties
/// break by run index, which makes the concatenation order a function of the run list and not of
/// the heap's internals; the sort that follows makes it unobservable either way.
struct MemberRunMerge {
    cursors: Vec<spill::MemberRunReader>,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(u32, usize)>>,
    index: u32,
    /// Every source the selected artifact carries, across all the runs holding it.
    sources: Vec<u64>,
}

impl MemberRunMerge {
    fn open(receipts: &[spill::SpillReceipt]) -> Result<MemberRunMerge> {
        let mut cursors = Vec::with_capacity(receipts.len());
        let mut heap = std::collections::BinaryHeap::with_capacity(receipts.len());
        for receipt in receipts {
            let mut reader = spill::MemberRunReader::open(receipt)?;
            // A run with no artifacts at all verifies its receipt here and takes no place in the
            // heap. The spill never writes one, but a receipt is a receipt.
            if reader.advance()? {
                heap.push(std::cmp::Reverse((reader.index(), cursors.len())));
            }
            cursors.push(reader);
        }
        Ok(MemberRunMerge {
            cursors,
            heap,
            index: 0,
            sources: Vec::new(),
        })
    }

    /// Select the next artifact, or `false` when every run is exhausted.
    fn next_artifact(&mut self) -> Result<bool> {
        let Some(&std::cmp::Reverse((index, _))) = self.heap.peek() else {
            return Ok(false);
        };
        self.index = index;
        self.sources.clear();
        while let Some(std::cmp::Reverse((head, run))) = self.heap.peek().copied() {
            if head != index {
                break;
            }
            self.heap.pop();
            let cursor = &mut self.cursors[run];
            let sources = &mut self.sources;
            cursor.take_sources(&mut |source| {
                sources.push(source);
                Ok(())
            })?;
            if cursor.advance()? {
                let next = cursor.index();
                self.heap.push(std::cmp::Reverse((next, run)));
            }
        }
        Ok(true)
    }

    fn index(&self) -> u32 {
        self.index
    }

    fn sources(&self) -> &[u64] {
        &self.sources
    }

    /// The same buffer, to be **rewritten in place** — a source id and the entity it resolves to
    /// are both `u64`, so the resolution is a rewrite and not a second vector. See
    /// [`merge_member_runs`], the one caller.
    fn sources_mut(&mut self) -> &mut [u64] {
        &mut self.sources
    }

    fn sort_sources(&mut self) {
        self.sources.sort_unstable();
    }
}

/// **The artifacts a `membership = { attribute = f }` layer holds**, keyed by layer name — one per
/// distinct value the column carries, in value-code order.
///
/// **The values are the roster, and the roster is what the column turned out to hold.** A predicate
/// layer publishes nothing, so its artifacts have to come from somewhere: they are the values, and
/// a value exists because a point carries it. That makes this a scan of the column rather than a
/// read of the declaration — an authored vocabulary value no point carries mints no artifact here,
/// exactly as an ingested value that has never appeared mints none until it does.
///
/// **Value-code order, so an ordinal is a function of the value.** Entity ids follow ordinals and
/// ordinals are identity, so an assignment that depended on which entity the scan met first would
/// move every artifact of the layer when a row moved in the input file. Ascending code is the same
/// stability rule `(layer, level, key)` order gives an enumerated layer's publication.
///
/// `codes_of` is *the distinct codes present in the attribute at this index*, which the two builds
/// answer from different structures — one holds typed entity columns and the other a scalar vector
/// per item — and which is the only thing about them this rule depends on.
pub fn predicate_artifact_keys(
    layers: &[LayerDeclaration],
    schema: &crate::config::Schema,
    minters: &std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    codes_of: &dyn Fn(usize) -> std::collections::BTreeSet<u32>,
) -> Result<BTreeMap<String, Vec<String>>> {
    use tessera_types::layer::{attribute_value_key, MembershipSource};
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for declaration in layers {
        let MembershipSource::Attribute(field) = &declaration.membership else {
            continue;
        };
        // The config refuses a membership naming a column no `[[attribute]]` declares, so reaching
        // here with one is a declaration that never validated rather than reachable input.
        let index = schema
            .attributes
            .iter()
            .position(|a| &a.name == field)
            .ok_or_else(|| {
                BuildError::Invalid(format!(
                    "layer '{}': `membership = {{ attribute = \"{field}\" }}` names no declared \
                     attribute",
                    declaration.name
                ))
            })?;
        let attribute = &schema.attributes[index];
        // `code → key`, from the declaration's own bindings **and** from whatever this run minted
        // into them: an open vocabulary's newest values live in the minter and its authored ones
        // do not, and an artifact named by a code where a key exists would be a second name for a
        // value the ingest route already knows by its key.
        let mut key_of_code: BTreeMap<u32, &str> = BTreeMap::new();
        if let Some(name) = &attribute.vocabulary {
            if let Some(vocabulary) = schema.vocabularies.get(name) {
                for (key, code) in &vocabulary.codes {
                    key_of_code.insert(*code, key.as_str());
                }
            }
            if let Some(minter) = minters.get(name) {
                for (key, code) in minter.bindings() {
                    key_of_code.insert(code, key);
                }
            }
        }
        let mut codes = codes_of(index);
        // **Code 0 is a category code space's reserved *absent* sentinel** and is never bound to a
        // key, so it names no value and no artifact. A plain integer column has no such
        // reservation and 0 is an ordinary value there.
        if attribute.vocabulary.is_some() {
            codes.remove(&tessera_store::vocabulary::ABSENT_CODE);
        }
        out.insert(
            declaration.name.clone(),
            codes
                .iter()
                .map(|code| attribute_value_key(*code, key_of_code.get(code).copied()))
                .collect(),
        );
    }
    Ok(out)
}

/// Every artifact of a layer that declares a dependency must declare one, into a layer that
/// layer named ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)).
///
/// **The same two refusals the control plane makes, made here where the input is a file.** A
/// dependent is served only where the artifact it attaches to is served, so an artifact carrying no
/// attachment has nothing for that prerequisite to gate on — and a build that admitted what an
/// ingest refuses is the fail-open half of one rule stated twice. What this adds over the registry's
/// own refusal is the address: the layer and key an operator has to go and fix, before the first
/// entity is allocated.
fn verify_dependencies(plan: &LayerPlan) -> Result<()> {
    let declared: BTreeMap<&str, &[String]> = plan
        .declarations
        .iter()
        .map(|d| (d.name.as_str(), d.depends_on.as_slice()))
        .collect();
    for ((layer, _, key), index) in &plan.artifacts {
        let Some(depends_on) = declared.get(layer.as_str()) else {
            continue;
        };
        match &plan.bodies[*index].attached_to {
            None if !depends_on.is_empty() => {
                return Err(BuildError::Invalid(format!(
                    "layer '{layer}' declares depends_on {depends_on:?}, so every artifact it \
                     publishes attaches to one — and {key} attaches to nothing. A dependent is \
                     visible only where what it depends on is visible, so an artifact with no \
                     dependency would be gated on nothing: give it attached_layer and \
                     attached_key, or drop depends_on from the layer"
                )));
            }
            Some(attachment) if !depends_on.contains(&attachment.layer) => {
                return Err(BuildError::Invalid(format!(
                    "layer '{layer}' publishes {key} attached into '{}', which it does not declare \
                     in depends_on {depends_on:?} — a dependency nobody declared is one no \
                     replacement checks, and one the publication order does not honour",
                    attachment.layer
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Check every declared parent/child edge, refusing the malformed and reporting the uncontained.
///
/// **The split between the two is which one a caller could have meant.** A parent key naming an
/// artifact that does not exist, a child claimed by two parents, a cycle — none of these describes
/// a tree at all, so there is nothing to publish and they refuse. A child holding a member its
/// parent does not is a *tree*, just one whose rollup guarantee does not hold on that branch; the
/// corpus may legitimately be that way, so it is named and published.
///
/// Edges relate artifacts **within one level** — a hierarchy's lineage lives in its edges and a
/// level is a resolution, so the two never carry each other
/// ([decision 0082](../../../docs/decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md)).
/// An edge naming a key in another level is therefore an unknown key here, and refuses.
type Address = (String, u32, String);

/// The buffers one containment pass reads a membership through: the extent's raw bytes, and the
/// members they decode to. Held by the task rather than by the parent, so the pass allocates once
/// per unit the work is split into and not once per artifact.
#[derive(Default)]
struct HierarchyBuffers {
    parent_bytes: Vec<u8>,
    parent_buf: Vec<u64>,
    child_bytes: Vec<u8>,
    child_buf: Vec<u64>,
}

fn verify_hierarchies(
    declarations: &[LayerDeclaration],
    index_of: &BTreeMap<Address, usize>,
    resolved: &[ResolvedArtifact],
    table: &spill::MemberTable,
) -> Result<(
    Vec<ContainmentViolation>,
    Vec<SplitCoverage>,
    Vec<HierarchyShape>,
)> {
    // Which parent has claimed each child, so a second claim is a refusal rather than a silent
    // reparenting: a child with two parents has two lineages, and which one a cut walks would
    // depend on iteration order. **A `dag` layer's child holds several, and never enters this
    // map** (`dag-hierarchies.md` §4); its containment and coverage are per edge below, exactly as
    // a tree's are.
    let mut claimed: BTreeMap<(&str, u32, &str), &str> = BTreeMap::new();
    // Children grouped under their parent, so containment and coverage are one pass over each
    // parent's membership rather than one per edge. **Each child by its full address**, because an
    // tiered layer's child sits at a different level from its parent and a bare key would
    // then be looked up in the wrong one — borrowed from the plan's own keys, so an edge costs a
    // pointer rather than the two heap `String`s an owned address did.
    let mut children_of: BTreeMap<&Address, Vec<&Address>> = BTreeMap::new();

    // Which shape each layer's edges have, from its declaration and never from the edges
    // themselves. A layer that declares no lineage may carry none; a nested layer's edges stay
    // within a level; a tiered layer's run from a coarser level to a finer one, and it is
    // the levels that carry the resolution rather than the edges.
    let kind_of: BTreeMap<&str, tessera_types::layer::HierarchyKind> = declarations
        .iter()
        .map(|d| (d.name.as_str(), d.hierarchy.kind))
        .collect();

    // The graph per treed level, counted as the edges are walked: every artifact of such a level
    // counts once, its parents each once.
    let mut shapes: BTreeMap<(&str, u32), HierarchyShape> = BTreeMap::new();
    for (address, index) in index_of {
        let (layer, level, key) = address;
        if let Some(kind) = kind_of.get(layer.as_str()).filter(|k| {
            !matches!(
                k,
                tessera_types::layer::HierarchyKind::Flat
                    | tessera_types::layer::HierarchyKind::Stacked
            )
        }) {
            let named = resolved[*index].parent_keys.len() as u64;
            let shape = shapes
                .entry((layer.as_str(), *level))
                .or_insert_with(|| HierarchyShape {
                    layer: layer.clone(),
                    level: *level,
                    kind: format!("{kind:?}").to_lowercase(),
                    artifacts: 0,
                    edges: 0,
                    roots: 0,
                    multi_parent: 0,
                    max_parents: 0,
                });
            shape.artifacts += 1;
            shape.edges += named;
            shape.roots += u64::from(named == 0);
            shape.multi_parent += u64::from(named > 1);
            shape.max_parents = shape.max_parents.max(named);
        }
        // Each parent an artifact names is one edge, checked on its own; the keys are already each
        // once, so a repeated edge cannot reach here.
        for parent_key in resolved[*index].parent_keys.iter().map(String::as_str) {
            let kind = kind_of
                .get(layer.as_str())
                .copied()
                .unwrap_or(tessera_types::layer::HierarchyKind::Flat);
            let cross_level = matches!(kind, tessera_types::layer::HierarchyKind::Tiered);
            let several = matches!(kind, tessera_types::layer::HierarchyKind::Dag);
            if !cross_level
                && !several
                && !matches!(kind, tessera_types::layer::HierarchyKind::Nested)
            {
                return Err(BuildError::Invalid(format!(
                "{layer} is declared {kind:?} and so has no lineage, but {key} names a parent — \
                 declare it nested if its edges run within a level, or tiered if they run between \
                 levels"
            )));
            }

            // **Where to look for the parent is the declared shape's to say.** A layer may not mix the
            // two directions, which is what makes an edge's meaning independent of the data: an
            // tiered layer's parent is in a strictly coarser level, and a key that resolves
            // only at this level or a finer one is an edge running against the resolution — refused
            // rather than reinterpreted.
            let parent_address = if cross_level {
                let mut found = None;
                for coarser in 0..*level {
                    let candidate = (layer.clone(), coarser, parent_key.to_string());
                    if let Some((stored, _)) = index_of.get_key_value(&candidate) {
                        if found.is_some() {
                            return Err(BuildError::Invalid(format!(
                            "{layer} artifact {key} names parent {parent_key}, which exists in \
                             more than one coarser level; which level the edge meant would depend \
                             on the search order, so it is refused rather than resolved"
                        )));
                        }
                        found = Some(stored);
                    }
                }
                match found {
                    Some(address) => address,
                    None => {
                        return Err(BuildError::Invalid(format!(
                        "{layer} level {level} artifact {key} names parent {parent_key}, which no \
                         coarser level declares — a tiered layer's edges run from a coarser \
                         level to a finer one, so a parent at this level or below is an \
                         edge running against the resolution"
                    )))
                    }
                }
            } else {
                let candidate = (layer.clone(), *level, parent_key.to_string());
                match index_of.get_key_value(&candidate) {
                    Some((stored, _)) => stored,
                    None => {
                        return Err(BuildError::Invalid(format!(
                        "{layer} level {level} artifact {key} names parent {parent_key}, which \
                         this level does not declare — a nested layer's edges relate two artifacts \
                         of one level, and a parent that does not exist would leave the child a \
                         root of a tree nobody wrote"
                    )))
                    }
                }
            };
            // **Only a within-level edge can name itself.** A key is unique per `(layer,
            // level)`, so a levelled taxonomy legitimately carries the same key at two levels — an
            // arXiv archive with no subclass is `hep-ph` at both, and the level-1 artifact's parent is
            // the level-0 one of the same name.
            if !cross_level && parent_key == key {
                return Err(BuildError::Invalid(format!(
                "{layer} level {level} artifact {key} names itself as its parent — the edges hold \
                 a cycle of length one"
            )));
            }
            if !several {
                if let Some(first) = claimed.insert((layer, *level, key), parent_key) {
                    return Err(BuildError::Invalid(format!(
                        "{layer} level {level} artifact {key} is claimed by both {first} and \
                     {parent_key}; a child has one lineage or the cut that walks it depends on \
                     iteration order. Declare `kind = \"dag\"` if a child may sit under several \
                     parents"
                    )));
                }
            }
            children_of.entry(parent_address).or_default().push(address);
        }
    }

    // **One parent per task, and every result taken back in the parents' own order.** A parent's
    // pass reads the merged member table and the resolved artifacts, and writes only the records
    // for that parent — there is no state between two parents to share. The map is indexed and
    // rayon's collect is ordered, so the violations and the coverage come back in the key order
    // `children_of` iterated in, and what a build prints does not move with the scheduler.
    //
    // ⊘ **Collecting `Result` directly.** Rayon keeps whichever error was stored first in time, so
    // a corpus with two unreadable extents would name a different one on different runs. The
    // results are collected and sequenced afterwards instead, which names the first in key order —
    // the one the serial pass named.
    let parents: Vec<(&&Address, &Vec<&Address>)> = children_of.iter().collect();
    let per_parent: Vec<Result<(Vec<ContainmentViolation>, SplitCoverage)>> = parents
        .into_par_iter()
        // **Two memberships resident per task, and never more**: the parent whose children are
        // being walked, and the child being walked. The buffers are the task's rather than the
        // parent's, so a pass over a hierarchy of 10⁵ artifacts allocates a handful of times as the
        // serial pass did, and the peak is a set of them per thread rather than one.
        .map_init(HierarchyBuffers::default, |scratch, (address, children)| {
            let (layer, level, parent_key) = *address;
            let parent_index = index_of[*address];
            load_members(
                &resolved[parent_index].members,
                parent_index,
                table,
                &mut scratch.parent_bytes,
                &mut scratch.parent_buf,
            )?;
            // **The parent's distinct members, in order** — the merge sorted them, so this is a
            // run-skip rather than a sort. It replaces a `HashSet<u64>` per parent, which for a
            // country-level division holding 2×10⁷ points was a ~300 MB table built and torn down,
            // with a second one beside it for what the children covered. The dedup is in place
            // because the buffer is this task's own: nothing else is reading it, and the largest
            // membership at the Overture rung is 73.6×10⁶ entries, which a copy would be 589 MB of.
            scratch.parent_buf.dedup();
            let held: &[u64] = &scratch.parent_buf;
            // One bit per distinct member, so `covered.len()` becomes a popcount: 2.5 MB where the
            // second `HashSet` was 300 MB, and the counts it feeds are identical by construction.
            let mut covered = vec![0u64; held.len().div_ceil(64)];
            let mut covered_count = 0u64;
            let mut violations = Vec::new();

            for child_address in children {
                let child_index = index_of[*child_address];
                load_members(
                    &resolved[child_index].members,
                    child_index,
                    table,
                    &mut scratch.child_bytes,
                    &mut scratch.child_buf,
                )?;
                let (_, child_level, child_key) = *child_address;
                let mut escaping = 0u64;
                // **Galloping from a cursor**, both sides being sorted: a child whose members sit
                // in one region of the parent's finds them in a few probes each rather than a full
                // binary search, and the walk is cache-resident where the hash table was not.
                let mut cursor = 0usize;
                for member in scratch.child_buf.iter().copied() {
                    if cursor < held.len() && held[cursor] > member {
                        cursor = 0;
                    }
                    let mut step = 1usize;
                    while cursor + step < held.len() && held[cursor + step] <= member {
                        step *= 2;
                    }
                    let hi = (cursor + step).min(held.len());
                    match held[cursor..hi].binary_search(&member) {
                        Ok(offset) => {
                            let at = cursor + offset;
                            cursor = at;
                            let (word, bit) = (at / 64, at % 64);
                            if covered[word] >> bit & 1 == 0 {
                                covered[word] |= 1u64 << bit;
                                covered_count += 1;
                            }
                        }
                        // Reported, not refused: the tree is real, its rollup guarantee is not.
                        Err(offset) => {
                            cursor = (cursor + offset).min(held.len().saturating_sub(1));
                            escaping += 1;
                        }
                    }
                }
                if escaping > 0 {
                    violations.push(ContainmentViolation {
                        layer: layer.clone(),
                        // The **child's** level, which is the one an operator needs to find it; for
                        // a nested layer it is the parent's too, and for a tiered one it is not.
                        level: *child_level,
                        child: child_key.clone(),
                        parent: parent_key.clone(),
                        escaping_members: escaping,
                    });
                }
            }

            Ok((
                violations,
                SplitCoverage {
                    layer: layer.clone(),
                    level: *level,
                    parent: parent_key.clone(),
                    children: children.len() as u32,
                    members: held.len() as u64,
                    stray_members: held.len() as u64 - covered_count,
                },
            ))
        })
        .collect();

    let mut violations = Vec::new();
    let mut coverage = Vec::with_capacity(per_parent.len());
    for parent in per_parent {
        let (escaped, covered) = parent?;
        violations.extend(escaped);
        coverage.push(covered);
    }

    detect_cycles(index_of, resolved, &kind_of)?;
    Ok((violations, coverage, shapes.into_values().collect()))
}

/// Refuse a hierarchy holding a cycle, which is neither a tree nor a DAG and has no root to descend
/// from.
///
/// A depth-first search over each artifact's parent lists, in key order, colouring each node once
/// it is known to reach a root; a node met while still on the chain being walked is the cycle
/// (`dag-hierarchies.md` §4). A self-edge is refused before this by `verify_hierarchies`.
///
/// **Only a nested or dag layer can hold one, and only their edges are walked.** A tiered layer's
/// edges each step to a strictly coarser level, and the levels are finite and bounded below by
/// zero, so a cycle is not expressible there.
///
/// **Skipping such a layer is required, not an optimisation.** Its keys are unique per level and
/// may legitimately repeat across them — an arXiv archive with no subclass is `hep-ph` at both —
/// so the same-level walk below would follow `hep-ph` at level 1 back to itself and report the
/// taxonomy as a cycle. That is exactly what it did before this guard existed, and the demo corpus
/// is what found it.
fn detect_cycles(
    index_of: &BTreeMap<Address, usize>,
    resolved: &[ResolvedArtifact],
    kind_of: &BTreeMap<&str, tessera_types::layer::HierarchyKind>,
) -> Result<()> {
    // One `key → parent` map per nested level, in key order. The walk below reads nothing else, so
    // building this once is what lets it borrow rather than clone an `Address` at every step — the
    // per-artifact walk allocated three `String`s per step and ran a step per ancestor.
    //
    // A `BTreeMap` at both levels, because the order artifacts are visited in is the order this
    // reports a cycle in, and that order must stay `artifacts.keys()`'s.
    let mut levels: BTreeMap<(&str, u32), BTreeMap<&str, &[String]>> = BTreeMap::new();
    for ((layer, level, key), index) in index_of {
        if !matches!(
            kind_of.get(layer.as_str()),
            Some(
                tessera_types::layer::HierarchyKind::Nested
                    | tessera_types::layer::HierarchyKind::Dag
            )
        ) {
            continue;
        }
        levels
            .entry((layer.as_str(), *level))
            .or_default()
            .insert(key.as_str(), resolved[*index].parent_keys.as_slice());
    }

    // **One visit per artifact, not one walk per artifact.** Each node is coloured once it is known
    // to reach a root, so a chain already proved good is left the moment it is re-entered — and a
    // node met while still on the current chain *is* the cycle, which is what the count bound was
    // standing in for. The bound it replaces was derived by scanning the whole map per artifact,
    // O(A²), and a `nested` layer puts every artifact at level 0 (`configuration.md`, the
    // hierarchy kinds): 600,000 artifacts at the Overture rung, 3.6×10¹¹ key visits, and the whole
    // of that build's layers stage. It also removes a latent hang — a corpus that genuinely held a
    // cycle ran the bound's full length for every artifact whose lineage reached it.
    //
    // **The artifact reported is the same one**: the walks start in key order and the first start
    // whose lineage reaches a cycle is the first artifact the counted walk would have failed on.
    // Over parent lists the walk is a depth-first search with an explicit stack of
    // `(node, next parent to try)`; on a tree every list has one entry and it is the chain walk
    // it replaces.
    for ((layer, level), parents) in &levels {
        // 0 unvisited · 1 on the chain being walked · 2 known to reach a root
        let mut state: std::collections::HashMap<&str, u8> =
            std::collections::HashMap::with_capacity(parents.len());
        for start in parents.keys() {
            if state.get(*start).copied().unwrap_or(0) == 2 {
                continue;
            }
            let mut chain: Vec<(&str, usize)> = vec![(*start, 0)];
            state.insert(*start, 1);
            while let Some((node, next)) = chain.last_mut() {
                let node = *node;
                let Some(parent) = parents[node].get(*next) else {
                    state.insert(node, 2);
                    chain.pop();
                    continue;
                };
                *next += 1;
                let parent = parent.as_str();
                // A parent this level does not hold ends that path: what sits above a key this
                // level never declared is not this level's to call a cycle.
                if !parents.contains_key(parent) {
                    continue;
                }
                match state.get(parent).copied().unwrap_or(0) {
                    2 => {}
                    1 => {
                        let from = chain
                            .iter()
                            .position(|(n, _)| *n == parent)
                            .expect("a node on the chain is in it");
                        let path: Vec<&str> = chain[from..]
                            .iter()
                            .map(|(n, _)| *n)
                            .chain(std::iter::once(parent))
                            .collect();
                        return Err(BuildError::Invalid(format!(
                            "{layer} level {level}: the lineage above {start} does not reach a \
                             root, so the edges hold a cycle — {} — and a hierarchy has a root \
                             to descend a cut from where a cycle has none",
                            path.join(" → ")
                        )));
                    }
                    _ => {
                        state.insert(parent, 1);
                        chain.push((parent, 0));
                    }
                }
            }
        }
    }
    Ok(())
}

/// One planned artifact with every source id resolved to the entity this build assigned it — and
/// its membership materialised, whichever way the source spelled it.
///
/// **The complement happens here and nowhere else.** `excluding` names the entities a membership
/// leaves out, and the set it means is *this build's entity space minus those* — `0..high_water`,
/// which is exactly the points this build assigned. Complementing once, here, is what makes the two
/// spellings produce the same bundle down to the byte: the store below is handed a list of members
/// either way, and nothing it writes records which way the caller wrote it. **There is no
/// request-time complement, and none is expressible**: a complement evaluated against a viewer's
/// mask rather than against the corpus would tell that viewer about the existence of items outside
/// it, so the spelling must not survive the build (`annotation-write-cycle.md` §6.1).
fn resolve_artifact(
    layer: &str,
    level: u32,
    key: &str,
    artifact: &mut PlannedArtifact,
    resolve: &(dyn Fn(u64) -> Option<u64> + Sync),
    high_water: u64,
) -> Result<ResolvedArtifact> {
    let refuse = |source: u64, what: &str| -> BuildError {
        BuildError::Invalid(format!(
            "{layer} level {level} artifact {key}: {what} names entity {source}, which this build \
             did not assign — the batch is refused rather than published without it, a dropped \
             member moving both the count a viewer is shown and the size a proportional criterion \
             divides by"
        ))
    };
    // **Rewritten where they sit.** A source id and the entity it resolves to are both `u64`, so
    // the plan's vector is the resolved one and no second allocation of the corpus's whole
    // membership exists to hold beside it.
    let in_place = |ids: &mut Vec<u64>, what: &str| -> Result<()> {
        for id in ids.iter_mut() {
            let source = *id;
            *id = resolve(source).ok_or_else(|| refuse(source, what))?;
        }
        Ok(())
    };

    let members = match std::mem::take(&mut artifact.membership) {
        // The member sources' rows are in the merged table by now, resolved and sorted by the
        // merge — there is nothing here to do for them and nothing here to hold.
        PlannedMembership::Rows => ResolvedMembers::Table,
        PlannedMembership::Included(mut ids) => {
            in_place(&mut ids, "membership")?;
            // **Sorted here, once, and not deduped.** Two readers want it in order — the
            // containment pass, which walks parent and child together instead of hashing a set per
            // parent, and `bitmap_of_entities`, which sorts before its bulk add. Deduping would be
            // wrong: a containment violation counts member *entries* that escape, duplicates
            // included, and that is the number an operator is given.
            ids.sort_unstable();
            ResolvedMembers::Inline(ids)
        }
        // **An excluded id this build did not assign refuses the build**, on the same rule an
        // unknown member does and for a sharper reason: an exclusion that resolves to nothing
        // silently *widens* the membership by the item it was meant to keep out.
        //
        // ⊘ **The complement is materialised in memory, and it is the one membership that still
        // is.** It is `high_water` entities minus a handful, so a corpus's worth of them could not
        // be held whichever side of the disk they sat — an `excluding` spelling is an authored one,
        // one row per artifact in a file somebody wrote, and the corpora that reach the spill do
        // not use it.
        PlannedMembership::Excluded(mut ids) => {
            in_place(&mut ids, "exclusion")?;
            let excluded: std::collections::HashSet<u64> = ids.into_iter().collect();
            ResolvedMembers::Inline(
                (0..high_water)
                    .filter(|entity| !excluded.contains(entity))
                    .collect(),
            )
        }
    };

    let mut contents = Vec::with_capacity(artifact.contents.len());
    for (rank, content) in artifact.contents.iter_mut().enumerate() {
        if content.values.is_empty() && content.generated_from.is_empty() {
            return Err(BuildError::Invalid(format!(
                "{layer} level {level} artifact {key}: contents[{rank}] is empty, so the ranking \
                 above it names a description that was never supplied"
            )));
        }
        in_place(&mut content.generated_from, &format!("contents[{rank}]"))?;
        contents.push(IncomingContent::new(
            std::mem::take(&mut content.values),
            std::mem::take(&mut content.generated_from)
                .into_iter()
                .map(EntityId::new),
        ));
    }

    Ok(ResolvedArtifact {
        view: artifact.view_key.take(),
        members,
        contents,
        attached_to: artifact.attached_to.take(),
        parent_keys: std::mem::take(&mut artifact.parent_keys),
        shape: artifact.shape.take(),
    })
}

/// What an artifact carries into the publication beyond its membership.
///
/// **Moved out of the plan before the memberships are read**, which is what lets the read and the
/// encode run over the plan rather than through it ([`publish`]): everything here is a `Vec` or an
/// `Option` the resolution already built, so taking it is a move rather than work.
struct PublishableBody {
    view: Option<String>,
    contents: Vec<IncomingContent>,
    attached_to: Option<IncomingAttachment>,
    parent_keys: Vec<String>,
    shape: Option<ArtifactShapes>,
}

impl ResolvedArtifact {
    /// Move this artifact's body out, leaving the membership where it is. The artifact is published
    /// once, so what is left behind is read by nothing.
    fn take_body(&mut self) -> PublishableBody {
        PublishableBody {
            view: self.view.take(),
            contents: std::mem::take(&mut self.contents),
            attached_to: self.attached_to.take(),
            parent_keys: std::mem::take(&mut self.parent_keys),
            shape: self.shape.take(),
        }
    }
}

/// The same artifact as the registry takes it. A membership is a list of entities by this point,
/// so there is nothing here to decide.
///
/// **The members are read into `buf` and turned into a bitmap here**, which is what keeps a level's
/// publication holding a level of *bitmaps* and never a level of entity vectors: the vector is one
/// artifact's, reused, and the `EntityId` newtype goes back on the raw ids at the one boundary that
/// leaves this module.
fn incoming_artifact(
    key: &str,
    body: PublishableBody,
    members: &ResolvedMembers,
    index: usize,
    table: &spill::MemberTable,
    scratch: &mut Vec<u8>,
    buf: &mut Vec<u64>,
) -> Result<IncomingArtifact> {
    load_members(members, index, table, scratch, buf)?;
    let entities = buf.iter().copied().map(EntityId::new);
    let mut result = match body.attached_to {
        None => IncomingArtifact::with_content(Some(key.to_string()), entities, body.contents),
        Some(attached_to) => {
            IncomingArtifact::attached(Some(key.to_string()), entities, body.contents, attached_to)
        }
    };
    result.parent_keys = body.parent_keys;
    result.shape = body.shape;
    result.view = body.view;
    Ok(result)
}

/// One artifact's members into `buf`, from wherever [`ResolvedMembers`] says they are — ascending,
/// duplicates kept.
///
/// `scratch` is the caller's byte buffer for the table's extent, reused across artifacts so a pass
/// over a level costs one allocation and not one per artifact.
fn load_members(
    members: &ResolvedMembers,
    index: usize,
    table: &spill::MemberTable,
    scratch: &mut Vec<u8>,
    buf: &mut Vec<u64>,
) -> Result<()> {
    match members {
        ResolvedMembers::Table => table.read_into(index, scratch, buf),
        ResolvedMembers::Inline(ids) => {
            buf.clear();
            buf.extend_from_slice(ids);
            Ok(())
        }
    }
}

/// The share of the build's memory budget one publication batch may hold.
///
/// A quarter. The publication runs between the join and the assembly, where the terms beside it
/// are the member table's reads and the store's mapped extents rather than anything anonymous, so
/// a quarter is headroom rather than a squeeze; and a batch larger than a few million entries buys
/// nothing, the work per artifact being the same in any batch and the batch already built across
/// the cores.
pub(crate) const PUBLICATION_BUDGET_SHARE: u64 = 4;

/// What one member entry costs while a batch is in flight: **24 bytes**.
///
/// Twelve measured for one copy — a level whose members are scattered across entity space is array
/// containers almost throughout, and 3.4×10⁹ entries came to 46 GB over two copies
/// (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §5) — and two copies
/// stand at once: the bitmaps [`incoming_artifact`] builds, and the copy `prepare_publish` takes
/// of each before the first is dropped.
pub(crate) const PUBLICATION_BYTES_PER_ENTRY: u64 = 24;

/// How many member entries one publication batch takes at `memory_budget` — the one arithmetic
/// [`publish`] cuts its batches by and [`crate::residency`] charges the stage at.
pub(crate) fn publication_batch_entries(memory_budget: u64) -> u64 {
    ((memory_budget / PUBLICATION_BUDGET_SHARE) / PUBLICATION_BYTES_PER_ENTRY).max(1)
}

/// One level's membership pack, written as the level was published and waiting to be named.
struct StreamedPack {
    path: PathBuf,
    ordinal_lo: u32,
    count: u32,
}

/// Whether a layer's hierarchy edges run **within** a level, which is what stops its levels being
/// published in batches.
///
/// `prepare_publish` resolves a parent key against the batch it is handed and the store beneath
/// it, so a child whose parent sits in a later batch of the same level would find nothing. A
/// nested layer's edges and a dag layer's are exactly the ones that can do that; a tiered layer's
/// parents sit at coarser levels, already published, and a flat or stacked layer has none.
fn within_level_edges(declarations: &[LayerDeclaration], layer: &str) -> bool {
    use tessera_types::layer::HierarchyKind;
    declarations
        .iter()
        .find(|d| d.name == layer)
        .map(|d| matches!(d.hierarchy.kind, HierarchyKind::Nested | HierarchyKind::Dag))
        .unwrap_or(true)
}

/// Where one level's publication is cut into batches: the exclusive end of each, ascending, the
/// last being the level's own length.
///
/// A batch takes artifacts in the level's key order until the next would put it over
/// `entries_per_batch`, and always takes at least one — an artifact larger than a whole batch is
/// published alone rather than refused, because it is one artifact's members and the alternative
/// is a corpus that cannot be built at all.
fn publication_batches(
    artifacts: &[(&str, usize)],
    resolved: &[ResolvedArtifact],
    table: &spill::MemberTable,
    entries_per_batch: u64,
    whole_level: bool,
) -> Vec<usize> {
    if whole_level || artifacts.is_empty() {
        return vec![artifacts.len()];
    }
    let mut splits = Vec::new();
    let mut entries = 0u64;
    for (position, (_, index)) in artifacts.iter().enumerate() {
        let of_this = match &resolved[*index].members {
            ResolvedMembers::Table => table.extent(*index).entries() as u64,
            ResolvedMembers::Inline(ids) => ids.len() as u64,
        };
        if entries > 0 && entries.saturating_add(of_this) > entries_per_batch {
            splits.push(position);
            entries = 0;
        }
        entries = entries.saturating_add(of_this);
    }
    splits.push(artifacts.len());
    splits
}

/// Pack every level's memberships into one extent, fsync it, and **read the store's copies back
/// through the mapped file** — the same format, one file per level, that a control-plane
/// publication writes.
///
/// **A blob at a time, into the file.** The store answers a level's ordinal *range*
/// (`pending_ranges`) and encodes an artifact's record where it stands (`encode_pending`), so what
/// is resident here is one blob and one extent's offset table at 8 bytes an artifact. Asking for
/// the blobs instead — which is what the online publication does, under a lock it may not hold
/// across an fsync — encodes every unpublished level of the corpus before the first byte is
/// written, and then `pack` concatenates each level again: two more copies of every membership in
/// the bundle, at the stage that is already the build's peak.
///
/// **And a mapping at a time, back out of it.** The store is carried from here to the artifact
/// pass, four stages later ([`PublishedLayers::store`]), holding one Roaring bitmap per artifact
/// over the corpus: **+1.2 GB of anonymous memory** at the 10⁷ MedCPT sample with 471,778,374
/// closed MeSH member rows, carried across the build's peak
/// (`probes/2026-09-02-mapped-memberships/README.md`). The bytes have just been written and
/// fsynced, so each record's bitmap is replaced by a view over the extent's own bytes
/// ([`tessera_lifecycle::Members::mapped`]) — one write, no second format, and what stays on the
/// heap is a container descriptor rather than the members.
///
/// ⊘ **A rehousing that does not take is reported and not refused.** Every failure route leaves
/// the heap bitmap the store already holds, which is the same membership answering the same
/// questions at the cost this exists to avoid — recoverable and disclosing nothing, so the build
/// prints the number and carries on (`CLAUDE.md`, *what the strictness is for*). The check that
/// *is* fail-closed is inside `rehouse_members`: a view whose cardinality differs from the bitmap
/// it would replace is refused, because a short membership is an artifact served as absent.
fn write_membership_extents(
    store: &mut ArtifactStore,
    prefix_dir: &Path,
    partition: &str,
    published: &mut PublishedLayers,
    streamed: &mut BTreeMap<(String, u32), StreamedPack>,
) -> Result<()> {
    let (ready, skipped) = store.pending_ranges();
    if let Some((layer, level)) = skipped.first() {
        // Unreachable from a build: every artifact of a level is published in one batch, so a
        // level cannot have a hole below its high-water. A refusal rather than an alarm, because a
        // build can simply not produce the bundle.
        return Err(BuildError::Invalid(format!(
            "{layer} level {level} has a hole in its ordinals, so its memberships cannot be packed \
             — an extent addresses a dense range and packing around a hole shifts every later \
             artifact's identity by one"
        )));
    }
    if ready.is_empty() {
        return Ok(());
    }
    let dir = prefix_dir
        .join("partitions")
        .join(partition)
        .join("members");
    std::fs::create_dir_all(&dir).map_err(|e| BuildError::io(&dir, e))?;
    let mut rehoused = 0u64;
    let mut kept = 0u64;
    for (index, (layer, level, ordinal_lo, count)) in ready.into_iter().enumerate() {
        // **The layer name never reaches the filename.** It is path-shaped — `clusters/a` — so a
        // name-derived path would escape the directory, or collide after escaping.
        let name = format!("members-000000-{index:03}.tsmb");
        let path = dir.join(&name);
        // **The level the publication streamed, renamed into the place its index names.** The
        // filename's index is the order `pending_ranges` answers in, which is the store's own key
        // order rather than the declaration order the publication ran in, so the pack is named
        // here and nowhere else.
        match streamed.remove(&(layer.clone(), level)) {
            Some(pack) if pack.ordinal_lo == ordinal_lo && pack.count == count => {
                std::fs::rename(&pack.path, &path).map_err(|e| BuildError::io(&path, e))?
            }
            // **A streamed pack that disagrees is a refusal, not a re-encode.** It used to fall
            // through to the arm below, which encodes from a store whose published memberships
            // have been vacated — the empty set — so the only thing standing between that and a
            // bundle of empty artifacts was the vacated-count check at the end of this function,
            // and a level every one of whose artifacts is legitimately empty would pass it. The
            // disagreement is a defect in the publication's own bookkeeping either way, and there
            // is nothing here to recover from it with.
            Some(pack) => {
                return Err(BuildError::Invalid(format!(
                    "{layer} level {level}: the publication streamed a membership pack over                      ordinals [{}, {}) and the store reports [{ordinal_lo}, {}) as ready to pack.                      The two must be the same range — the pack is what the extent will address",
                    pack.ordinal_lo,
                    pack.ordinal_lo as u64 + pack.count as u64,
                    ordinal_lo as u64 + count as u64
                )));
            }
            // A level the publication did not stream — a predicate layer's, derived and applied
            // before that loop runs — is encoded from the store here, which is where every level
            // was encoded before the publication was batched.
            None => {
                let mut writer =
                    tessera_store::membership::PackWriter::create(&path, ordinal_lo, count)
                        .map_err(BuildError::Store)?;
                let mut pushed = 0u32;
                for blob in store.encode_pending(&layer, level, ordinal_lo, count) {
                    // Unreachable: `pending_ranges` reports a level with a hole as skipped above
                    // rather than as a range. A refusal rather than an assertion because the
                    // alternative is an extent one blob short of the range it addresses, which
                    // serves every ordinal above the hole as another artifact's membership.
                    let blob = blob.ok_or_else(|| {
                        BuildError::Invalid(format!(
                            "{layer} level {level} has no record at an ordinal inside the range \
                             it reported as ready to pack"
                        ))
                    })?;
                    writer.push(&blob).map_err(BuildError::Store)?;
                    pushed += 1;
                }
                if pushed != count {
                    return Err(BuildError::Invalid(format!(
                        "{layer} level {level}: {pushed} membership(s) were encoded for a range of                          {count}, so the extent would address records that are not there"
                    )));
                }
                writer.finish().map_err(BuildError::Store)?;
            }
        }
        rehoused += map_level_memberships(store, &path, &layer, level, &mut kept)?;
        crate::trim_heap();
        published.paths.push(path);
        published.membership_extents.push(MembershipExtent {
            path: format!("partitions/{partition}/members/{name}"),
            layer,
            level,
            ordinal_lo,
            count,
        });
    }
    // A pack nothing claimed is a level the publication streamed and `pending_ranges` did not
    // report — which cannot happen, every published level being pending in a build. Removed
    // rather than left, so no unnamed file stands in the bundle.
    for (_, pack) in std::mem::take(streamed) {
        let _ = std::fs::remove_file(&pack.path);
    }
    tessera_store::fsync_dir(&dir).map_err(BuildError::Store)?;
    // **A vacated artifact holds the empty set** ([`ArtifactStore::vacate_members`]), so one left
    // standing is an artifact this bundle would serve as absent. The heap copy the publication
    // encoded from is gone by now, so there is nothing to fall back to and the build refuses.
    if store.vacated_count() > 0 {
        return Err(BuildError::Invalid(format!(
            "{} published membership(s) could not be read back through the extent this build just \
             wrote, and the publication no longer holds them: the bundle would serve those \
             artifacts as absent with nothing reporting it",
            store.vacated_count()
        )));
    }
    if kept > 0 {
        eprintln!(
            "layers: {kept} of {} membership(s) stayed on the heap rather than being read back \
             through the extent this build just wrote; every answer is unchanged and the build \
             holds those bitmaps to the end of its run",
            kept + rehoused
        );
    }
    Ok(())
}

/// One extent's memberships, read back through the mapped file and put into the store in place of
/// the bitmaps they were encoded from. Returns how many took; `kept` counts the rest.
///
/// The pack is held by every view it hands out — an `Arc` per extent, cloned into each `Members` —
/// so the mapping outlives the store exactly as far as the store's records reach.
fn map_level_memberships(
    store: &mut ArtifactStore,
    path: &Path,
    layer: &str,
    level: u32,
    kept: &mut u64,
) -> Result<u64> {
    let pack = std::sync::Arc::new(
        tessera_store::membership::MembershipPack::open(path).map_err(BuildError::Store)?,
    );
    let owner: std::sync::Arc<dyn std::any::Any + Send + Sync> = pack.clone();
    let mut rehoused = 0;
    for (ordinal, blob) in pack.iter() {
        // SAFETY: `blob` is a slice of `pack`'s read-only mapping, `owner` is that same pack, and
        // the `Members` this produces holds `owner` for as long as it holds the view. The file is
        // written, fsynced and closed for writing before this runs, and nothing in the build
        // reopens it for writing.
        let mapped = tessera_lifecycle::membership::members_bytes(blob)
            .and_then(|bytes| unsafe { tessera_lifecycle::Members::mapped(bytes, owner.clone()) });
        let took = match mapped {
            Some(members) => store.rehouse_members(layer, level, ordinal, members),
            None => false,
        };
        match took {
            true => rehoused += 1,
            false => *kept += 1,
        }
    }
    Ok(rehoused)
}

/// Write every artifact's supplied content as one record-blob extent — the store points use, in an
/// extent of its own.
///
/// The storage is shared and the access rule is not: a document's field is visible to whoever may
/// see the document, an artifact's content to whoever contains its generating set. Sharing is safe
/// because the two never share an entity, artifact ids descending from the ceiling while point ids
/// ascend from zero, so which rule governs a row is a range check on its id.
fn write_content_extent(
    store: &ArtifactStore,
    prefix_dir: &Path,
    partition: &str,
    published: &mut PublishedLayers,
) -> Result<()> {
    let mut rows = store.unpublished_content();
    if rows.is_empty() {
        return Ok(());
    }
    // **No multi-partition refusal here, and that is a property of the build rather than an
    // omission**: a batch build writes exactly one partition, so the online path's refusal — an
    // artifact belongs to no partition, and writing its content into each would give one record
    // stack two layers with overlapping has-row bitmaps — has no case to fire on. It becomes this
    // function's problem the day a build writes two, and the argument lives on the online copy.
    let extents_rel = format!("partitions/{partition}/attrs/record/extents");
    let dir = prefix_dir.join(&extents_rel);
    std::fs::create_dir_all(&dir).map_err(|e| BuildError::io(&dir, e))?;
    let extent = RecordExtent {
        blocks: format!("{extents_rel}/artifacts-000000.blocks.bin"),
        hasrow: format!("{extents_rel}/artifacts-000000.hasrow.roaring"),
        directory: format!("{extents_rel}/artifacts-000000.directory.arrow"),
    };
    let blocks = prefix_dir.join(&extent.blocks);
    let hasrow = prefix_dir.join(&extent.hasrow);
    let directory = prefix_dir.join(&extent.directory);
    let mut writer = tessera_filter_write::RecordBlobWriter::create(
        &blocks,
        &hasrow,
        &directory,
        tessera_filter::RECORD_BLOCK_TARGET,
    )
    .map_err(|e| BuildError::io(&blocks, e))?;
    // **Ascending by entity**, which the blob's block directory requires. Artifact ids descend as
    // they are allocated, so publication order is exactly the wrong order here and sorting is not
    // an optimisation.
    rows.sort_by_key(|(entity, _)| entity.raw());
    for (entity, fields) in rows {
        let entity = u32::try_from(entity.raw()).map_err(|_| {
            BuildError::Invalid(format!(
                "artifact entity {} does not fit the u32 entity space (I9's ceiling)",
                entity.raw()
            ))
        })?;
        let fields: Vec<tessera_filter::RecordField> = fields
            .into_iter()
            .map(|(tag, value)| tessera_filter::RecordField {
                tag,
                value: tessera_filter::RecordValue::Utf8(value),
            })
            .collect();
        writer
            .push_row(entity, &fields)
            .map_err(|e| BuildError::io(&blocks, e))?;
    }
    writer.finish().map_err(|e| BuildError::io(&blocks, e))?;
    // `finish` syncs the blocks and not the two files that address them, and a torn addressing file
    // refuses the **whole** record stack at open — every point's blob-resident field with it.
    for path in [&hasrow, &directory] {
        let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
        file.sync_all().map_err(|e| BuildError::io(path, e))?;
    }
    tessera_store::fsync_dir(&dir).map_err(BuildError::Store)?;
    published.paths.extend([blocks, hasrow, directory]);
    published.artifact_record_extents.push(extent);
    Ok(())
}

// ---- Parquet column access ----------------------------------------------------------------
//
// Narrow helpers rather than a general reader: these two files have a handful of columns each,
// every one of which is named here, so a mistyped column is a refusal naming the column rather
// than a silent absence.
//
// **Every field is read under the name the declaration resolved** ([`Fields`]), and the split
// between the two kinds of miss is `configuration.md` §8's. A field the map *named* and the file
// does not carry is a refusal spelling out the object, the field, the column looked for and the
// columns the file has — the reader's half of the rule, needing the file open. A field nothing
// named and the file does not carry is simply absent, which is what an optional column of the
// artifact grain is.

/// The two fields no `fields` map may move, because `configuration.md` §1's table does not name
/// them: a level is an address rather than a value, and the map's key set is the closed one that
/// table states.
const LEVEL: &str = "level";
const ATTACHED_LEVEL: &str = "attached_level";

pub(crate) fn batches(
    path: &Path,
) -> Result<impl Iterator<Item = Result<arrow::record_batch::RecordBatch>> + '_> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| BuildError::parquet(path, e))?
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    Ok(reader.map(move |batch| batch.map_err(|e| BuildError::arrow(path, e))))
}

/// How many decoded batches the read-ahead may hold beyond the one each side is working on.
///
/// A batch is the parquet reader's own default of 1,024 rows, which on the member file's shape is
/// ~120 kB of Arrow: an entity per row and a list of three keys beside it. Thirty-two of them is a
/// few megabytes against a build whose peak is gigabytes. The depth is there to amortise the
/// handoff and not to buffer the file — a queue one deep trades a futex with the consumer 10⁵ times
/// over a 10⁸-row file, and the file's own size is bounded by nothing this could hold.
const READ_AHEAD_BATCHES: usize = 32;

/// [`batches`], decoded on a second thread and handed to the caller in file order.
///
/// The decode is ZSTD, and on `gbif-240p` it is 9.1 s in front of a member read whose row loop is
/// 27 s — one thread's work sitting ahead of another thread's
/// (`probes/2026-09-09-layers-cost/README.md`). Reading ahead overlaps the two. The queue is
/// bounded, so the producer runs ahead only as far as [`READ_AHEAD_BATCHES`] and what the read
/// holds is a constant rather than the file.
///
/// **The caller sees the batches in file order, and each error at the batch that raised it**, so
/// nothing the row loop does with them moves. The order is not only an output question: a member
/// key that misses the roster mints an artifact, and which key mints first is the file's order
/// ([`read_members`]).
///
/// **A panic in the decode ends the build.** The queue closing is what the caller reads as the end
/// of the file, and a producer that died mid-file closes it the same way — so the iterator joins
/// the thread when the queue closes and resumes the panic there. Without that, a decode that died
/// would publish a bundle carrying the members it managed to read, with the count of them as the
/// only trace.
fn batches_ahead(path: &Path, depth: usize) -> Result<ReadAhead> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(depth);
    let owned = path.to_path_buf();
    let producer = std::thread::Builder::new()
        .name("member-decode".to_string())
        .spawn(move || match batches(&owned) {
            // The open and the footer read happen on this thread too, so their refusal travels the
            // queue like any other and the caller reads it where it happened: first.
            Err(e) => {
                let _ = sender.send(Err(e));
            }
            Ok(reader) => {
                for batch in reader {
                    if sender.send(batch).is_err() {
                        break;
                    }
                }
            }
        })
        .map_err(|e| BuildError::io(path, e))?;
    Ok(ReadAhead {
        receiver: Some(receiver),
        producer: Some(producer),
    })
}

/// The consumer half of [`batches_ahead`].
struct ReadAhead {
    /// Dropped when the queue ends or the caller stops early, which is what tells the producer to
    /// stop decoding.
    receiver: Option<std::sync::mpsc::Receiver<Result<arrow::record_batch::RecordBatch>>>,
    producer: Option<std::thread::JoinHandle<()>>,
}

impl Iterator for ReadAhead {
    type Item = Result<arrow::record_batch::RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(batch) = self.receiver.as_ref().and_then(|queue| queue.recv().ok()) {
            return Some(batch);
        }
        self.receiver = None;
        if let Some(producer) = self.producer.take() {
            if let Err(panic) = producer.join() {
                std::panic::resume_unwind(panic);
            }
        }
        None
    }
}

impl Drop for ReadAhead {
    /// A caller that stopped early — a refusal in the row loop — closes the queue and waits for the
    /// producer to notice it. The join's result is dropped here: the caller is already carrying the
    /// error worth reporting, and raising a second panic while one unwinds aborts the process.
    fn drop(&mut self) {
        self.receiver = None;
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

/// Every column the batch carries, for a refusal to spell out.
fn column_names(batch: &arrow::record_batch::RecordBatch) -> String {
    let schema = batch.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// A required field, under the name the declaration resolved.
fn required<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<&'a std::sync::Arc<dyn Array>> {
    let name = fields.of(canonical);
    batch.column_by_name(name).ok_or_else(|| {
        BuildError::Invalid(format!(
            "{}: {} reads field `{canonical}` from a column named '{name}', which this file does \
             not carry. Its columns are: {}",
            path.display(),
            fields.object(),
            column_names(batch)
        ))
    })
}

/// A field that may be absent — unless the declaration named it, in which case its absence is the
/// refusal `configuration.md` §8 puts on the readers.
fn optional<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a std::sync::Arc<dyn Array>>> {
    let name = fields.of(canonical);
    match batch.column_by_name(name) {
        Some(array) => Ok(Some(array)),
        None if fields.names(canonical) => Ok(Some(required(path, batch, fields, canonical)?)),
        None => Ok(None),
    }
}

fn typed<'a, T: 'static>(
    path: &Path,
    array: &'a std::sync::Arc<dyn Array>,
    name: &str,
) -> Result<&'a T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        BuildError::Invalid(format!(
            "{}: column {name} is {:?}, which this reader cannot take",
            path.display(),
            array.data_type()
        ))
    })
}

fn optional_utf8<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a StringArray>> {
    match optional(path, batch, fields, canonical)? {
        None => Ok(None),
        Some(array) => typed(path, array, fields.of(canonical)).map(Some),
    }
}

fn u64s<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<&'a UInt64Array> {
    typed(
        path,
        required(path, batch, fields, canonical)?,
        fields.of(canonical),
    )
}

/// A `u32` column read under its own name — the two the field map may not move.
fn optional_u32<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<&'a UInt32Array>> {
    match batch.column_by_name(name) {
        None => Ok(None),
        Some(array) => typed(path, array, name).map(Some),
    }
}

fn optional_u32_field<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a UInt32Array>> {
    match optional(path, batch, fields, canonical)? {
        None => Ok(None),
        Some(array) => typed(path, array, fields.of(canonical)).map(Some),
    }
}

fn optional_list<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a ListArray>> {
    match optional(path, batch, fields, canonical)? {
        None => Ok(None),
        Some(array) => typed(path, array, fields.of(canonical)).map(Some),
    }
}

/// The membership list on an artifact row, included or excluded.
fn optional_u64_list<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a ListArray>> {
    optional_list(path, batch, fields, canonical)
}

/// The ranked `contents` on an artifact row: a list of entries, each a list of values.
fn optional_ranked_values<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a ListArray>> {
    optional_list(path, batch, fields, canonical)
}

fn value_at(column: &StringArray, row: usize) -> Option<String> {
    (!column.is_null(row)).then(|| column.value(row).to_string())
}

fn number_at(column: &UInt32Array, row: usize) -> u32 {
    if column.is_null(row) {
        0
    } else {
        column.value(row)
    }
}

fn value_index(column: &UInt32Array, row: usize) -> Option<u32> {
    (!column.is_null(row)).then(|| column.value(row))
}

/// One row's membership, as source entity ids.
///
/// **A null element is a refusal rather than entity zero**, on the member source's own rule: Arrow
/// reads the values buffer whatever the validity bitmap says, so a producer whose join missed a row
/// would otherwise publish the corpus's lowest-numbered document into the artifact.
fn u64s_at(path: &Path, column: &ListArray, row: usize, key: &str) -> Result<Vec<u64>> {
    if column.is_null(row) {
        return Ok(Vec::new());
    }
    let values = column.value(row);
    let ids = values
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            BuildError::Invalid(format!(
            "{}: the membership of {key} is a list of {:?}, and this reader takes a list of uint64",
            path.display(),
            values.data_type()
        ))
        })?;
    (0..ids.len())
        .map(|i| {
            if ids.is_null(i) {
                return Err(BuildError::Invalid(format!(
                    "{}: {key} has a null entity in its membership; a null is not entity zero, and \
                     publishing it as one puts a document nobody named into the artifact",
                    path.display()
                )));
            }
            Ok(ids.value(i))
        })
        .collect()
}

/// One row's ranked contents: entry *k* is `contents[k]`'s values, one per supplied kind.
///
/// **The ranking is one cell, so there is no rank column and no gap to detect.** Its position in
/// the list *is* the rank, which is what the `(artifact, rank)` grain needed a dense integer column
/// to say — and what a missing row in that grain could silently renumber.
fn ranked_at(
    path: &Path,
    column: &ListArray,
    row: usize,
    key: &str,
) -> Result<Vec<PlannedContent>> {
    if column.is_null(row) {
        return Ok(Vec::new());
    }
    let entries = column.value(row);
    let entries = entries
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| {
            BuildError::Invalid(format!(
            "{}: the contents of {key} are a list of {:?}, and this reader takes a list of lists \
             of utf8 — one entry per rank, each carrying a value per supplied kind",
            path.display(),
            entries.data_type()
        ))
        })?;
    (0..entries.len())
        .map(|rank| {
            if entries.is_null(rank) {
                return Err(BuildError::Invalid(format!(
                    "{}: contents[{rank}] of {key} is null, so the ranking above it names a \
                     description that was never supplied",
                    path.display()
                )));
            }
            Ok(PlannedContent {
                values: strings_at(path, entries, rank, key)?,
                generated_from: Vec::new(),
            })
        })
        .collect()
}

/// One entry's content values.
///
/// **Every failure here is a refusal rather than a shorter list.** A null element read as `""`
/// serves an artifact with an empty description — the in-between state decision 0076 forbids,
/// reached past the publication check, which counts values rather than reading them. A child array
/// this reader cannot take would produce *no* values, which the count check does refuse, but with a
/// message about arity that sends an operator looking in the wrong place.
fn strings_at(path: &Path, column: &ListArray, row: usize, key: &str) -> Result<Vec<String>> {
    if column.is_null(row) {
        return Ok(Vec::new());
    }
    let values = column.value(row);
    let strings = values
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            BuildError::Invalid(format!(
                "{}: the values of {key} are a list of {:?}, and this reader takes a list of utf8",
                path.display(),
                values.data_type()
            ))
        })?;
    (0..strings.len())
        .map(|i| {
            if strings.is_null(i) {
                return Err(BuildError::Invalid(format!(
                    "{}: {key} supplies a null value; an artifact is served with every kind its \
                     layer declares or it is not served at all, so a null here would be an empty \
                     description rather than a withheld artifact",
                    path.display()
                )));
            }
            Ok(strings.value(i).to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Key columns: text, or an integer spelling one
// ---------------------------------------------------------------------------------------------

/// A `key` column at the two types a producer has — UTF-8, or an integer canonicalised to its
/// decimal string, so `3` and `"3"` name one artifact.
///
/// **A key is one type below this reader**: the plan, the store and the manifest all hold a string,
/// so an integer column is converted rather than carried. *Where* the conversion happens is the
/// whole of this type's design — **once per artifact, never once per point.** Formatting a member
/// row's key costs ~54 s at 10⁹ against ~15 s for an integer hash and ~0.5 s where the value is
/// already an interned code (measured, 10⁵ distinct ids, single-threaded), and cluster ids are
/// integers, so the common case would pay the worst of the three. The member pass therefore
/// converts the **roster** once into [`KeyRoster`] and looks each point up by the key it
/// already has: a point whose cluster is on the roster formats nothing and allocates nothing, and
/// the only decimal string an open layer writes is the one it mints an artifact under, once per
/// cluster.
#[derive(Clone, Copy)]
pub(crate) enum KeyColumn<'a> {
    Text(&'a StringArray),
    I8(&'a Int8Array),
    I16(&'a Int16Array),
    I32(&'a Int32Array),
    I64(&'a Int64Array),
    U8(&'a UInt8Array),
    U16(&'a UInt16Array),
    U32(&'a UInt32Array),
    U64(&'a UInt64Array),
}

/// What one **member** row's key says.
enum KeyRead<'a> {
    /// **This point is in no artifact** — a null key, or exactly `-1`, the sentinel every clusterer
    /// emits for noise (`artifacts-from-points.md` §2). Exactly `-1` and not any negative: a
    /// negative id is otherwise unusual enough that swallowing `-7` would more likely be eating
    /// data than handling noise. For a text column, null only.
    Unclustered,
    Named(&'a str),
    Numbered(i128),
}

impl<'a> KeyColumn<'a> {
    fn array(&self) -> &dyn Array {
        match self {
            KeyColumn::Text(a) => *a as &dyn Array,
            KeyColumn::I8(a) => *a as &dyn Array,
            KeyColumn::I16(a) => *a as &dyn Array,
            KeyColumn::I32(a) => *a as &dyn Array,
            KeyColumn::I64(a) => *a as &dyn Array,
            KeyColumn::U8(a) => *a as &dyn Array,
            KeyColumn::U16(a) => *a as &dyn Array,
            KeyColumn::U32(a) => *a as &dyn Array,
            KeyColumn::U64(a) => *a as &dyn Array,
        }
    }

    fn integer_at(&self, row: usize) -> Option<i128> {
        match self {
            KeyColumn::Text(_) => None,
            KeyColumn::I8(a) => Some(a.value(row) as i128),
            KeyColumn::I16(a) => Some(a.value(row) as i128),
            KeyColumn::I32(a) => Some(a.value(row) as i128),
            KeyColumn::I64(a) => Some(a.value(row) as i128),
            KeyColumn::U8(a) => Some(a.value(row) as i128),
            KeyColumn::U16(a) => Some(a.value(row) as i128),
            KeyColumn::U32(a) => Some(a.value(row) as i128),
            KeyColumn::U64(a) => Some(a.value(row) as i128),
        }
    }

    /// The canonical key at `row` — **the allocating read, for one row per artifact.**
    fn key_at(&self, row: usize) -> Option<String> {
        if self.array().is_null(row) {
            return None;
        }
        Some(match self {
            KeyColumn::Text(a) => a.value(row).to_string(),
            _ => self.integer_at(row).expect("an integer column").to_string(),
        })
    }

    /// What one member row's key says — **the non-allocating read, for one row per point.**
    ///
    /// The noise sentinel is [`tessera_types::layer::NOISE_KEY`]'s, not a literal here: the wire
    /// reads the same cell out of an Arrow batch and the two must agree about what `-1` means.
    fn read_at(&self, row: usize) -> KeyRead<'a> {
        if self.array().is_null(row) {
            return KeyRead::Unclustered;
        }
        match self {
            KeyColumn::Text(a) => KeyRead::Named(a.value(row)),
            _ => match self.integer_at(row).expect("an integer column") {
                tessera_types::layer::NOISE_KEY => KeyRead::Unclustered,
                value => KeyRead::Numbered(value),
            },
        }
    }
}

pub(crate) fn key_column<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<KeyColumn<'a>> {
    let name = fields.of(canonical);
    scalar_key_column(
        path,
        required(path, batch, fields, canonical)?,
        name,
        "column",
    )
}

/// One array read as a key column — the member source's own, or the elements of its list.
fn scalar_key_column<'a>(
    path: &Path,
    array: &'a std::sync::Arc<dyn Array>,
    name: &str,
    what: &str,
) -> Result<KeyColumn<'a>> {
    Ok(match array.data_type() {
        arrow::datatypes::DataType::Utf8 => KeyColumn::Text(typed(path, array, name)?),
        arrow::datatypes::DataType::Int8 => KeyColumn::I8(typed(path, array, name)?),
        arrow::datatypes::DataType::Int16 => KeyColumn::I16(typed(path, array, name)?),
        arrow::datatypes::DataType::Int32 => KeyColumn::I32(typed(path, array, name)?),
        arrow::datatypes::DataType::Int64 => KeyColumn::I64(typed(path, array, name)?),
        arrow::datatypes::DataType::UInt8 => KeyColumn::U8(typed(path, array, name)?),
        arrow::datatypes::DataType::UInt16 => KeyColumn::U16(typed(path, array, name)?),
        arrow::datatypes::DataType::UInt32 => KeyColumn::U32(typed(path, array, name)?),
        arrow::datatypes::DataType::UInt64 => KeyColumn::U64(typed(path, array, name)?),
        other => {
            return Err(BuildError::Invalid(format!(
                "{}: {what} {name} is {other:?}, and a key is text or an integer — an integer key \
                 is read as its decimal spelling, so `3` and \"3\" name one artifact",
                path.display()
            )))
        }
    })
}

// ---------------------------------------------------------------------------------------------
// The artifact row's `parent`: one key, or a list of them
// ---------------------------------------------------------------------------------------------

/// An artifact row's `parent` column: a scalar key, or a list of keys — `list<utf8>` or
/// `list<int>` on the rule an integer key already takes — which a `dag` layer's child needs and
/// every kind accepts, a scalar being a list of one (`dag-hierarchies.md` §4). The row's grain
/// stays one row per artifact: several parents are several entries in one cell, and the one-row
/// refusal in [`read_artifacts`] stands.
pub(crate) enum ParentColumn<'a> {
    Scalar(KeyColumn<'a>),
    Listed(ListShape<'a>, KeyColumn<'a>),
}

pub(crate) fn parent_column<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
) -> Result<Option<ParentColumn<'a>>> {
    let Some(array) = optional(path, batch, fields, "parent")? else {
        return Ok(None);
    };
    let name = fields.of("parent");
    Ok(Some(match array.data_type() {
        arrow::datatypes::DataType::List(_) => {
            let list: &ListArray = typed(path, array, name)?;
            ParentColumn::Listed(
                ListShape::Variable(list),
                scalar_key_column(path, list.values(), name, "the elements of")?,
            )
        }
        arrow::datatypes::DataType::FixedSizeList(..) => {
            let list: &FixedSizeListArray = typed(path, array, name)?;
            ParentColumn::Listed(
                ListShape::Fixed(list),
                scalar_key_column(path, list.values(), name, "the elements of")?,
            )
        }
        _ => ParentColumn::Scalar(scalar_key_column(path, array, name, "column")?),
    }))
}

/// One row's parents, each key once in the order written; empty where the cell is null, which is
/// a root. An integer is its decimal spelling, as a key is everywhere.
pub(crate) fn parents_at(
    path: &Path,
    column: Option<&ParentColumn<'_>>,
    row: usize,
) -> Result<Vec<String>> {
    let mut keys = match column {
        None => Vec::new(),
        Some(ParentColumn::Scalar(key)) => key.key_at(row).into_iter().collect(),
        Some(ParentColumn::Listed(shape, values)) => {
            let range = match shape {
                ListShape::Variable(list) => {
                    if list.is_null(row) {
                        return Ok(Vec::new());
                    }
                    let offsets = list.value_offsets();
                    offsets[row] as usize..offsets[row + 1] as usize
                }
                ListShape::Fixed(list) => {
                    if list.is_null(row) {
                        return Ok(Vec::new());
                    }
                    let width = list.value_length() as usize;
                    row * width..(row + 1) * width
                }
            };
            range
                .map(|index| {
                    values.key_at(index).ok_or_else(|| {
                        BuildError::Invalid(format!(
                            "{}: row {row}'s parent list holds a null entry; a parent is named \
                             or the entry is left out, and a null here would be an edge to \
                             nothing",
                            path.display()
                        ))
                    })
                })
                .collect::<Result<Vec<String>>>()?
        }
    };
    dedup_keys(&mut keys);
    Ok(keys)
}

// ---------------------------------------------------------------------------------------------
// A list key column: the artifacts a point belongs to, and the edges between them
// ---------------------------------------------------------------------------------------------

/// A member source's `key` column: one artifact per row, or a list of them.
///
/// **The list's meaning is the hierarchy kind the layer already declares**
/// (`artifacts-from-points.md` §4). A hierarchical clusterer emits a list per point and nothing in
/// the list says what its positions mean, so the kind is declared as it always was and only the
/// edges are read from the data.
enum MemberKeys<'a> {
    Scalar(KeyColumn<'a>),
    Listed(ListedKeys<'a>),
}

/// A list key column, its elements, and what its positions mean.
struct ListedKeys<'a> {
    shape: ListShape<'a>,
    /// The list's child array, read as a key column: an element is a key on exactly the rule a
    /// scalar is, integer or text, with the roster converted once rather than per element.
    values: KeyColumn<'a>,
    meaning: ListMeaning,
}

pub(crate) enum ListShape<'a> {
    /// A `List`: its rows may differ in length, which is what a lineage is.
    Variable(&'a ListArray),
    /// A `FixedSizeList`: every row has the arity the type states.
    Fixed(&'a FixedSizeListArray),
}

impl ListedKeys<'_> {
    /// The row's entries, as a range into the element array — `None` where the row named no
    /// artifact at all.
    ///
    /// **A null cell and an empty one are the whole row's `Unclustered`**, which is §2's rule for a
    /// scalar key applied to a cell that holds no key: a point may be in no artifact at any
    /// resolution, and a clusterer that emitted nothing for it is the ordinary way of saying so.
    fn entries(
        &self,
        path: &Path,
        layer: &str,
        row: usize,
    ) -> Result<Option<std::ops::Range<usize>>> {
        let (start, end) = match &self.shape {
            ListShape::Variable(list) => {
                if list.is_null(row) {
                    return Ok(None);
                }
                let offsets = list.value_offsets();
                (offsets[row] as usize, offsets[row + 1] as usize)
            }
            ListShape::Fixed(list) => {
                if list.is_null(row) {
                    return Ok(None);
                }
                let start = list.value_offset(row) as usize;
                (start, start + list.value_length() as usize)
            }
        };
        if start == end {
            return Ok(None);
        }
        // **The declaration and the data must agree.** A `stacked` or `tiered` layer's list is one
        // entry per declared level — that is what makes entry *k* mean level *k* — so a row of any
        // other length is a lineage against a levelled declaration, and guessing which of the two
        // the caller meant would publish a hierarchy they did not write.
        if let ListMeaning::Levelled { levels, .. } = self.meaning {
            if end - start != levels {
                return Err(BuildError::Invalid(format!(
                    "{}: row {row} names {} artifacts and layer '{layer}' declares {levels} \
                     levels. A stacked or tiered layer's key column is one entry per level, \
                     nullable where the point is in no artifact at that resolution, so a row of \
                     another length is a variable-length list against a levelled declaration — \
                     declare `hierarchy.kind = \"nested\"` if the column is a lineage",
                    path.display(),
                    end - start,
                )));
            }
        }
        Ok(Some(start..end))
    }
}

/// The member source's key column, at the shapes a layer of this kind may carry.
fn member_keys<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    fields: &Fields,
    layer: &str,
    declaration: &LayerDeclaration,
) -> Result<MemberKeys<'a>> {
    let name = fields.of("key");
    let array = required(path, batch, fields, "key")?;
    let fixed = match array.data_type() {
        arrow::datatypes::DataType::List(_) => None,
        arrow::datatypes::DataType::FixedSizeList(_, size) => Some(*size as usize),
        _ => return Ok(MemberKeys::Scalar(key_column(path, batch, fields, "key")?)),
    };
    // **The meaning of the positions is the layer's own declaration**, read through the one rule
    // both entry points share ([`ListMeaning`]). What is left here is the *type* half of the arity
    // check: an Arrow `FixedSizeList` states its length in its own type, which a plain list does
    // not, so it is the one place a disagreement can be caught before a row is read.
    let meaning = declaration.list_meaning();
    if let Some(size) = fixed {
        match meaning {
            ListMeaning::Lineage => {
                return Err(BuildError::Invalid(format!(
                    "{}: column {name} is a fixed-size list of {size} and layer '{layer}' is \
                     declared nested, whose lineage is as deep as each point's own branch — a \
                     fixed arity is one entry per level, which is the stacked and tiered shape. \
                     Write the column as a list, or declare the layer tiered and its levels",
                    path.display()
                )))
            }
            ListMeaning::Levelled { levels, .. } if size != levels => {
                return Err(BuildError::Invalid(format!(
                    "{}: column {name} is a fixed-size list of {size} and layer '{layer}' \
                     declares {levels} levels. Entry k is the artifact at level k, so the two \
                     counts are one number written twice",
                    path.display()
                )))
            }
            _ => {}
        }
    }
    let shape = match array.data_type() {
        arrow::datatypes::DataType::List(_) => {
            ListShape::Variable(typed::<ListArray>(path, array, name)?)
        }
        _ => ListShape::Fixed(typed::<FixedSizeListArray>(path, array, name)?),
    };
    let values = match &shape {
        ListShape::Variable(list) => list.values(),
        ListShape::Fixed(list) => list.values(),
    };
    Ok(MemberKeys::Listed(ListedKeys {
        values: scalar_key_column(path, values, name, "the elements of column")?,
        shape,
        meaning,
    }))
}

/// The layer's artifacts, indexed by what their keys spell — the integer, and the text.
///
/// **Built once, before the first member row is read**, which is what keeps the per-point path free
/// of formatting *and* of allocation. A point's integer key is looked up as the integer it already
/// is; a text key is looked up as a borrowed `&str`. Either way the answer is an index into the
/// plan's own arena, so a row's several keys are held at once as `usize`s and the artifact they
/// name is written through without a second lookup.
///
/// **The text half is why the Overture rung's layers stage was what it was.** Before it, every
/// member entry built `(layer.to_string(), level, key.to_string())` and probed a `BTreeMap` whose
/// comparison walks two heap `String`s — three times over, counting the membership write and the
/// lineage — at 3×10⁸ entries.
struct KeyRoster {
    by_integer: BTreeMap<(u32, i128), usize>,
    /// `level → key → plan index`, nested so the lookup borrows: a `HashMap<Box<str>, _>` answers
    /// a `&str`, where a `(u32, String)` tuple key would allocate on every probe — which is the
    /// whole point of interning the text path.
    ///
    /// **`FxHashMap`, because this is probed once per member entry** and the standard hasher is
    /// SipHash — a keyed hash whose HashDoS resistance buys nothing over an artifact roster that
    /// came out of the caller's own file. The map is probed and never iterated, so the weaker
    /// hasher's ordering is unobservable.
    by_text: BTreeMap<u32, rustc_hash::FxHashMap<Box<str>, usize>>,
}

impl KeyRoster {
    /// **Built once, over the layer's whole planned roster**, integer and text keys alike.
    fn of_layer(layer: &str, plan: &LayerPlan) -> KeyRoster {
        let mut roster = KeyRoster {
            by_integer: BTreeMap::new(),
            by_text: BTreeMap::new(),
        };
        for (address, index) in plan.artifacts.iter().filter(|(a, _)| a.0 == layer) {
            if let Some(value) = canonical_integer(&address.2) {
                roster.by_integer.insert((address.1, value), *index);
            }
            roster
                .by_text
                .entry(address.1)
                .or_default()
                .insert(address.2.as_str().into(), *index);
        }
        roster
    }

    fn get(&self, level: u32, value: i128) -> Option<usize> {
        self.by_integer.get(&(level, value)).copied()
    }

    fn text(&self, level: u32, key: &str) -> Option<usize> {
        self.by_text.get(&level)?.get(key).copied()
    }

    fn insert(&mut self, level: u32, value: i128, address: Address, index: usize) -> usize {
        self.by_integer.insert((level, value), index);
        self.by_text
            .entry(level)
            .or_default()
            .insert(address.2.as_str().into(), index);
        index
    }

    fn insert_text(&mut self, level: u32, key: &str, address: Address, index: usize) -> usize {
        if let Some(value) = canonical_integer(&address.2) {
            self.by_integer.insert((level, value), index);
        }
        self.by_text
            .entry(level)
            .or_default()
            .insert(key.into(), index);
        index
    }
}

/// The integer a key spells, where it spells one **exactly**.
///
/// `007` and ` 7` are keys that no integer column can produce, so they index nothing here and are
/// matched by nothing — which is the property that keeps one artifact from having two addresses.
fn canonical_integer(key: &str) -> Option<i128> {
    let value: i128 = key.parse().ok()?;
    (value.to_string() == key).then_some(value)
}

/// A member key the layer's artifacts do not declare, under a closed value set.
fn undeclared_key(path: &Path, address: &Address) -> BuildError {
    BuildError::Invalid(format!(
        "{}: names {} in level {} of {}, which the layer's artifacts do not declare. Declare \
         `value_set = \"open\"` on the layer for a key the artifacts omit to create one",
        path.display(),
        address.2,
        address.1,
        address.0
    ))
}

/// One row's `(layer, level, key)`.
///
/// **The layer is the source's own**, never a column: one file holds one layer, which is what
/// removes the discriminator and with it any way for a layer to ingest another's rows.
///
/// **A key is required**, and its absence is a refusal rather than a generated name: an artifact
/// published without one can be named by no edge and matched by no later build, and the caller is
/// the only party who knows what it should be called.
/// The key at `row`, or its row number where the row names none — for the check's report, where
/// a nameless row is reported rather than refused.
pub(crate) fn key_at(key: &KeyColumn, row: usize) -> String {
    key.key_at(row).unwrap_or_else(|| format!("row {row}"))
}

fn address(
    path: &Path,
    layer: &str,
    level: &Option<&UInt32Array>,
    key: &KeyColumn,
    row: usize,
) -> Result<Address> {
    let Some(key) = key.key_at(row) else {
        return Err(BuildError::Invalid(format!(
            "{}: row {row} names no key; a build-published artifact carries the caller's own name \
             for it, which is what an edge into it names",
            path.display()
        )));
    };
    Ok((
        layer.to_string(),
        level.map_or(0, |c| number_at(c, row)),
        key,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ArtifactSource, InlineArtifact, LayerSources};
    use tessera_lifecycle::membership::ArtifactShapes;
    use tessera_spatial::{AlignedSquare, Bounds, Projection};
    use tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES;

    /// A triangle whose diagonal is straight in the longitude/latitude plane and a curve in the
    /// frame — 8°W 50°N → 2°E 58°N → 8°W 58°N, the fixture `projected_build.rs` measures the two
    /// readings apart on. It is in degrees, so on a projected view it is a `wgs84` declaration or
    /// nothing: read as frame coordinates it falls wholly outside the `[0, 1]` unit square.
    const UK: &str = "POLYGON ((-8 50, 2 58, -8 58, -8 50))";

    /// A layer whose membership *is* the polygon, and one that merely draws it, declared as alike
    /// as the two can be: one view, one polygon, one key, one space.
    fn two_layers(space: Option<&str>) -> (Vec<LayerDeclaration>, Vec<LayerSources>) {
        let common = serde_json::json!({
            "views": ["world"],
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
        });
        let mut selects = common.clone();
        selects["name"] = serde_json::json!("regions/selects");
        selects["membership"] = serde_json::json!("spatial");
        selects["shape"] = serde_json::json!({ "kind": "polygon" });
        let mut draws = common;
        draws["name"] = serde_json::json!("regions/draws");
        draws["membership"] = serde_json::json!("enumerated");
        draws["content"] = serde_json::json!({
            "computed": [],
            "supplied": [{
                "name": "outline",
                "type": "polygon",
                "require_member_visibility": "inherited"
            }]
        });
        let declarations = [selects, draws]
            .into_iter()
            .map(|d| serde_json::from_value(d).expect("the fixture declaration is well-formed"))
            .collect();

        let row = |shape: serde_json::Value| -> InlineArtifact {
            let mut row = serde_json::json!({ "key": "uk", "space": space });
            for (k, v) in shape.as_object().unwrap() {
                row[k] = v.clone();
            }
            serde_json::from_value(row).expect("the fixture row is well-formed")
        };
        let sources = vec![
            LayerSources {
                name: "regions/selects".to_string(),
                artifacts: Some(ArtifactSource::Inline(vec![row(
                    serde_json::json!({ "wkt": UK }),
                )])),
                members: None,
            },
            LayerSources {
                name: "regions/draws".to_string(),
                artifacts: Some(ArtifactSource::Inline(vec![row(
                    serde_json::json!({ "contents": [[UK]] }),
                )])),
                members: None,
            },
        ];
        (declarations, sources)
    }

    /// The frame coordinates each layer's `uk` ended up on: the membership shape's canonical
    /// bytes, and the authored content's read back out of the slot it was written into.
    fn canonical_pair(
        space: Option<&str>,
        projection: Projection,
        extent: Bounds,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let (declarations, sources) = two_layers(space);
        // These fixtures exercise the shape path and never reach the member spill; the scratch
        // directory and budget are what `read` needs to construct one at all.
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let plan = read(
            &declarations,
            &sources,
            &BTreeMap::new(),
            &[tessera_store::derived::ViewFrame::new(
                "world", projection, extent,
            )],
            DEFAULT_MAX_SHAPE_VERTICES,
            scratch.path(),
            1 << 30,
        )?;
        let body = |layer: &str| -> &PlannedArtifact {
            let index = plan.artifacts[&(layer.to_string(), 0, "uk".to_string())];
            &plan.bodies[index]
        };
        let selects = body("regions/selects")
            .shape
            .as_ref()
            .expect("the membership shape")
            .for_view("world")
            .expect("the one view")
            .to_vec();
        let draws = ArtifactShapes::from_content_text(&body("regions/draws").contents[0].values[0])
            .expect("the authored slot holds canonical bytes, not the caller's WKT")
            .for_view("world")
            .expect("the one view")
            .to_vec();
        Ok((selects, draws))
    }

    fn world() -> Bounds {
        AlignedSquare::WORLD.bounds()
    }

    fn unprojected() -> Bounds {
        Bounds {
            x_min: 0.0,
            x_max: 1000.0,
            y_min: 0.0,
            y_max: 1000.0,
        }
    }

    /// **An authored shape is read in the space its row declares, exactly as the membership shape
    /// beside it is** (`polygon-membership.md` §6.1): the same degrees in through both, the same
    /// canonical bytes out. A drawing and the membership it came from are written in one
    /// coordinate system by one producer, so a build that projected the second and not the first
    /// would place a ±180 × ±90 outline in a corner of the `[0, 1]` frame — refusing nothing and
    /// reporting nothing, R12's degrees-looking report being blind on a projected view.
    #[test]
    fn an_authored_shape_is_read_in_the_space_its_row_declares() {
        let (selects, draws) = canonical_pair(Some("wgs84"), Projection::WebMercator, world())
            .expect("a `wgs84` polygon canonicalises on a projected view");
        assert_eq!(selects, draws);
        // And both reached the frame: read as view coordinates these degrees are wholly outside
        // the unit square, and the equality above would hold with the pair collapsed to nothing.
        assert!(
            selects.len() > 64,
            "the densified triangle is {} bytes; it did not reach the frame",
            selects.len()
        );
    }

    /// **A shape in view space is what it has always been**, whichever kind declares it — the
    /// regression guard on every authored shape written before a space could be declared at all.
    /// It holds under either reading of the space, which is the point: it is what must not move.
    #[test]
    fn an_authored_shape_in_view_space_is_unchanged() {
        let (selects, draws) = canonical_pair(None, Projection::None, unprojected())
            .expect("a view-space polygon canonicalises on any view");
        assert_eq!(selects, draws);
        // `space = "view"` written out is the same declaration as none written at all.
        let (_, spelled) = canonical_pair(Some("view"), Projection::None, unprojected())
            .expect("the default spelled out");
        assert_eq!(draws, spelled);
    }

    /// **A published membership is read back through the extent this build has just written**, and
    /// answers exactly what the heap bitmap it replaced did.
    ///
    /// The guard that matters is the *equality*: a view over the wrong bytes would be a membership
    /// whose masked count is low for every viewer, which the existence criterion renders as absent
    /// with nothing anywhere to notice. The `is_mapped` assertion is the second half — without it
    /// the rehousing could quietly stop taking and only a memory measurement would ever say so.
    #[test]
    fn a_published_membership_is_read_back_through_the_extent_it_was_written_to() {
        let declaration: LayerDeclaration = serde_json::from_value(serde_json::json!({
            "name": "clusters/a",
            "views": ["world"],
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "membership": "enumerated",
        }))
        .expect("the fixture declaration is well-formed");
        let row = |key: &str, members: &[u64]| -> InlineArtifact {
            serde_json::from_value(serde_json::json!({ "key": key, "members": members }))
                .expect("the fixture row is well-formed")
        };
        let sources = vec![LayerSources {
            name: "clusters/a".to_string(),
            artifacts: Some(ArtifactSource::Inline(vec![
                row("a", &[1, 2, 3, 900]),
                row("b", &[4, 5]),
            ])),
            members: None,
        }];
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let prefix = tempfile::tempdir().expect("a prefix directory");
        let mut plan = read(
            std::slice::from_ref(&declaration),
            &sources,
            &BTreeMap::new(),
            &[tessera_store::derived::ViewFrame::new(
                "world",
                Projection::None,
                AlignedSquare::WORLD.bounds(),
            )],
            DEFAULT_MAX_SHAPE_VERTICES,
            scratch.path(),
            1 << 30,
        )
        .expect("an enumerated layer with two inline artifacts reads");
        let published = publish(
            &mut plan,
            &|source| Some(source),
            1_000,
            prefix.path(),
            "default",
            &["world".to_string()],
            &BTreeMap::new(),
        )
        .expect("two artifacts publish");

        let members: Vec<(u32, Vec<u32>, bool)> = published
            .store
            .level("clusters/a", 0)
            .map(|(ordinal, record)| {
                (
                    ordinal,
                    record.members.iter().collect(),
                    record.members.is_mapped(),
                )
            })
            .collect();
        assert_eq!(
            members,
            vec![(0, vec![1, 2, 3, 900], true), (1, vec![4, 5], true),],
            "each membership must be the set it was published with, read through the mapping"
        );
    }

    /// The drawing layer on its own, so that a refusal below is the authored path's own and not
    /// the membership shape beside it reaching the same check first.
    /// **The mix of a projected view and an embedding is refused at the layer's declaration**
    /// (decision 0111), before a row is read — so the message names the layer and both views and
    /// not a key nobody asked about.
    #[test]
    fn a_shape_layer_over_a_projected_and_an_unprojected_view_is_refused_by_name() {
        let (mut declarations, sources) = two_layers(Some("wgs84"));
        for declaration in &mut declarations {
            declaration.views = vec!["world".to_string(), "embedding".to_string()];
        }
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let error = read(
            &declarations,
            &sources,
            &BTreeMap::new(),
            &[
                tessera_store::derived::ViewFrame::new(
                    "world",
                    Projection::WebMercator,
                    AlignedSquare::WORLD.bounds(),
                ),
                tessera_store::derived::ViewFrame::new(
                    "embedding",
                    Projection::None,
                    Bounds {
                        x_min: -40.0,
                        x_max: 40.0,
                        y_min: -40.0,
                        y_max: 40.0,
                    },
                ),
            ],
            DEFAULT_MAX_SHAPE_VERTICES,
            scratch.path(),
            1 << 30,
        )
        .err()
        .expect("no geometry spans the two kinds of space");
        let message = error.to_string();
        assert!(message.contains("regions/selects"), "{message}");
        assert!(message.contains("'world'"), "{message}");
        assert!(message.contains("'embedding'"), "{message}");
    }

    fn draws_only(wkt: &str, projection: Projection, extent: Bounds) -> BuildError {
        let (declarations, mut sources) = two_layers(Some("wgs84"));
        sources.retain(|s| s.name == "regions/draws");
        sources[0].artifacts = Some(ArtifactSource::Inline(vec![serde_json::from_value(
            serde_json::json!({ "key": "uk", "space": "wgs84", "contents": [[wkt]] }),
        )
        .expect("the fixture row is well-formed")]));
        let scratch = tempfile::tempdir().expect("a scratch directory");
        read(
            &declarations,
            &sources,
            &BTreeMap::new(),
            &[tessera_store::derived::ViewFrame::new(
                "world", projection, extent,
            )],
            DEFAULT_MAX_SHAPE_VERTICES,
            scratch.path(),
            1 << 30,
        )
        .err()
        .expect("the declaration is refused")
    }

    /// A `wgs84` authored shape on a view with no projection is refused naming the view's own
    /// declaration: such a view has one space and nothing to convert a degree from
    /// (`polygon-membership.md` §4.3). The refusal is the membership path's, reached because the
    /// space now reaches it.
    #[test]
    fn an_authored_wgs84_shape_on_an_unprojected_view_is_refused() {
        let message = draws_only(UK, Projection::None, unprojected()).to_string();
        assert!(message.contains("`projection` is `none`"), "{message}");
        assert!(message.contains("regions/draws"), "{message}");
    }

    /// A `wgs84` coordinate outside ±180 × ±90 is not a coordinate, and is refused where the
    /// authored shape is read (`projections.md` §2) — not clamped to the frame as a view-space
    /// coordinate would be.
    #[test]
    fn an_authored_wgs84_coordinate_outside_the_range_is_refused() {
        let outside = "POLYGON ((-8 50, 2 91, -8 91, -8 50))";
        let message = draws_only(outside, Projection::WebMercator, world()).to_string();
        assert!(message.contains("not a coordinate"), "{message}");
    }

    /// One run from a list of `(artifact, sources)` records.
    fn run(dir: &Path, seq: usize, records: &[(u32, Vec<u64>)]) -> spill::SpillReceipt {
        let mut writer =
            spill::MemberRunWriter::create(&dir.join(format!("run-{seq:04}.spill"))).unwrap();
        for (index, sources) in records {
            writer.push(*index, sources).unwrap();
        }
        writer.finish().unwrap()
    }

    /// Drain a merge into `(artifact, sources)` records, sorting each artifact's sources — which
    /// is what both callers do, one before writing an intermediate run and one after resolving.
    fn drain(receipts: &[spill::SpillReceipt]) -> Vec<(u32, Vec<u64>)> {
        let mut merge = MemberRunMerge::open(receipts).unwrap();
        let mut out = Vec::new();
        while merge.next_artifact().unwrap() {
            merge.sort_sources();
            out.push((merge.index(), merge.sources().to_vec()));
        }
        out
    }

    /// **An artifact is yielded once, with every run's sources for it, and duplicates survive.**
    /// The merge is what makes a membership out of a stream, so a pair it drops is a masked count
    /// short and a pair it collapses is a containment report that disagrees with the operator's
    /// own file.
    #[test]
    fn the_merge_gathers_an_artifact_from_every_run_that_holds_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let receipts = vec![
            run(
                temp.path(),
                0,
                &[(0, vec![5, 9]), (2, vec![1]), (7, vec![4])],
            ),
            run(temp.path(), 1, &[(0, vec![3]), (7, vec![4, 6])]),
            run(temp.path(), 2, &[(1, vec![8])]),
        ];
        assert_eq!(
            drain(&receipts),
            vec![
                (0, vec![3, 5, 9]),
                (1, vec![8]),
                (2, vec![1]),
                // Named by both runs, and the pair is two member entries rather than one.
                (7, vec![4, 4, 6]),
            ]
        );
    }

    /// **The merge's output is a function of the runs' contents and not of their order.** A k-way
    /// merge is where an ordering assumption gets made implicitly, and an ordinal that moved with
    /// one would be a corpus whose ids depend on how the spill happened to window — which I9 does
    /// not allow.
    #[test]
    fn the_merge_yields_the_same_membership_whatever_order_the_runs_are_in() {
        let temp = tempfile::TempDir::new().unwrap();
        let records: Vec<Vec<(u32, Vec<u64>)>> = vec![
            vec![(0, vec![5, 9]), (3, vec![1, 1])],
            vec![(0, vec![3]), (1, vec![7]), (3, vec![2])],
            vec![(1, vec![0]), (3, vec![4])],
        ];
        let forward: Vec<spill::SpillReceipt> = records
            .iter()
            .enumerate()
            .map(|(seq, r)| run(temp.path(), seq, r))
            .collect();
        let reversed: Vec<spill::SpillReceipt> = records
            .iter()
            .rev()
            .enumerate()
            .map(|(seq, r)| run(temp.path(), 10 + seq, r))
            .collect();
        assert_eq!(drain(&forward), drain(&reversed));
    }

    /// **The cascade is the same merge, and it holds every pair across a reduction.** Nothing
    /// measured reaches it — GeoNames spills two runs against a fan-in of 128 — so the only
    /// exercise it gets is this one, and it feeds the memberships every masked count divides by.
    #[test]
    fn a_cascade_reduces_the_runs_and_loses_no_pair() {
        let temp = tempfile::TempDir::new().unwrap();
        // Two full passes' worth: each run names three artifacts drawn from a space small enough
        // that every artifact is in most runs, which is the case a cascade has to gather.
        let runs = MEMBER_MERGE_FAN_IN * 2 + 3;
        let receipts: Vec<spill::SpillReceipt> = (0..runs)
            .map(|seq| {
                let records: Vec<(u32, Vec<u64>)> = (0..3)
                    .map(|i| ((seq as u32 + i) % 5, vec![seq as u64, seq as u64]))
                    .collect::<std::collections::BTreeMap<u32, Vec<u64>>>()
                    .into_iter()
                    .collect();
                run(temp.path(), seq, &records)
            })
            .collect();
        let direct = drain(&receipts);
        let cascaded = cascade_member_runs(&receipts, temp.path()).unwrap();
        assert!(cascaded.len() <= MEMBER_MERGE_FAN_IN, "the cascade reduces");
        assert_eq!(drain(&cascaded), direct);
        assert_eq!(
            direct.iter().map(|(_, s)| s.len() as u64).sum::<u64>(),
            receipts.iter().map(|r| r.count).sum::<u64>(),
            "every pair the runs held came out of the cascade"
        );
    }

    /// The read-ahead's whole contract: the same batches, in the same order, however deep the queue
    /// is. The depth of one is the case where every handoff blocks, which is where an ordering
    /// mistake would show.
    #[test]
    fn a_read_ahead_yields_the_file_s_batches_in_the_file_s_order() {
        use arrow::array::UInt64Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let temp = tempfile::tempdir().expect("a scratch directory");
        let path = temp.path().join("rows.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("entity", DataType::UInt64, false)]));
        let mut writer = parquet::arrow::ArrowWriter::try_new(
            File::create(&path).expect("create the file"),
            schema.clone(),
            None,
        )
        .expect("open the writer");
        // Several row groups of several batches each, so the queue fills and drains more than once.
        for group in 0..4u64 {
            let values: Vec<u64> = (0..5_000).map(|row| group * 5_000 + row).collect();
            let batch = arrow::record_batch::RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(UInt64Array::from(values))],
            )
            .expect("a batch");
            writer.write(&batch).expect("write the batch");
            writer.flush().expect("close the row group");
        }
        writer.close().expect("close the file");

        let read = |rows: Vec<arrow::record_batch::RecordBatch>| -> Vec<u64> {
            rows.iter()
                .flat_map(|batch| {
                    typed::<UInt64Array>(&path, batch.column(0), "entity")
                        .expect("a u64 column")
                        .iter()
                        .map(|v| v.expect("no nulls"))
                        .collect::<Vec<u64>>()
                })
                .collect()
        };
        let serial: Vec<arrow::record_batch::RecordBatch> =
            batches(&path).unwrap().map(|b| b.unwrap()).collect();
        assert_eq!(read(serial.clone()), (0..20_000).collect::<Vec<u64>>());
        for depth in [1usize, 2, READ_AHEAD_BATCHES] {
            let ahead: Vec<arrow::record_batch::RecordBatch> = batches_ahead(&path, depth)
                .unwrap()
                .map(|b| b.unwrap())
                .collect();
            assert_eq!(
                ahead.iter().map(|b| b.num_rows()).collect::<Vec<usize>>(),
                serial.iter().map(|b| b.num_rows()).collect::<Vec<usize>>(),
                "queue depth {depth} changed the batch boundaries"
            );
            assert_eq!(read(ahead), read(serial.clone()), "queue depth {depth}");
        }
    }

    /// **The containment pass reports its findings in the parents' order, whatever order the
    /// parents were walked in.** Each parent's pass runs on whichever thread rayon gives it, so a
    /// result appended as it finished would put the violations and the coverage in an order that
    /// changed between two builds of the same corpus. Enough parents that the work is split several
    /// ways, and the same call repeated, because a scheduling fault shows on some runs and not on
    /// others.
    #[test]
    fn the_containment_pass_reports_in_the_parents_order_and_not_the_schedulers() {
        let declaration: LayerDeclaration = serde_json::from_value(serde_json::json!({
            "name": "clusters/a",
            "views": ["world"],
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "nested", "prune_children": false },
            "membership": "enumerated",
        }))
        .expect("the fixture declaration is well-formed");

        let parents = 240usize;
        let mut index_of: BTreeMap<Address, usize> = BTreeMap::new();
        let mut resolved: Vec<ResolvedArtifact> = Vec::new();
        let body = |members: Vec<u64>, parent_keys: Vec<String>| ResolvedArtifact {
            view: None,
            members: ResolvedMembers::Inline(members),
            contents: Vec::new(),
            attached_to: None,
            parent_keys,
            shape: None,
        };
        for parent in 0..parents {
            let member = parent as u64 * 10;
            index_of.insert(
                ("clusters/a".to_string(), 0, format!("p{parent:03}")),
                resolved.len(),
            );
            resolved.push(body(vec![member], Vec::new()));
            index_of.insert(
                ("clusters/a".to_string(), 0, format!("c{parent:03}")),
                resolved.len(),
            );
            // One member the parent holds and one it does not, so every parent has exactly one
            // violation to report and exactly one covered member.
            resolved.push(body(
                vec![member, 1_000_000 + parent as u64],
                vec![format!("p{parent:03}")],
            ));
        }
        let table = spill::MemberTable::empty(resolved.len());

        let expected: Vec<(String, String)> = (0..parents)
            .map(|parent| (format!("p{parent:03}"), format!("c{parent:03}")))
            .collect();
        for attempt in 0..5 {
            let (violations, coverage, _) = verify_hierarchies(
                std::slice::from_ref(&declaration),
                &index_of,
                &resolved,
                &table,
            )
            .expect("the fixture hierarchy is well-formed");
            assert_eq!(
                violations
                    .iter()
                    .map(|v| (v.parent.clone(), v.child.clone()))
                    .collect::<Vec<(String, String)>>(),
                expected,
                "attempt {attempt}: the violations are not in the parents' order"
            );
            assert!(
                violations.iter().all(|v| v.escaping_members == 1),
                "attempt {attempt}: each child escapes its parent by one member"
            );
            assert_eq!(
                coverage.iter().map(|c| c.parent.clone()).collect::<Vec<String>>(),
                expected.iter().map(|(parent, _)| parent.clone()).collect::<Vec<String>>(),
                "attempt {attempt}: the coverage is not in the parents' order"
            );
            assert!(
                coverage.iter().all(|c| c.members == 1 && c.stray_members == 0),
                "attempt {attempt}: each parent's one member is covered by its child"
            );
        }
    }

    /// A caller that stops before the file ends closes the queue, and the producer notices: the
    /// drop returns rather than waiting for a file it will never finish handing over.
    #[test]
    fn a_read_ahead_dropped_part_way_stops_its_producer() {
        use arrow::array::UInt64Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let temp = tempfile::tempdir().expect("a scratch directory");
        let path = temp.path().join("rows.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("entity", DataType::UInt64, false)]));
        let mut writer = parquet::arrow::ArrowWriter::try_new(
            File::create(&path).expect("create the file"),
            schema.clone(),
            None,
        )
        .expect("open the writer");
        let values: Vec<u64> = (0..200_000).collect();
        writer
            .write(
                &arrow::record_batch::RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(UInt64Array::from(values))],
                )
                .expect("a batch"),
            )
            .expect("write the batch");
        writer.close().expect("close the file");

        let mut ahead = batches_ahead(&path, 1).unwrap();
        assert!(ahead.next().is_some(), "the first batch arrives");
        drop(ahead);
    }

    /// The batch cut: entries, not artifacts, and never an empty batch.
    #[test]
    fn a_level_is_cut_where_the_entries_run_out_and_never_before_one_artifact() {
        let sizes = [3u64, 3, 3, 10, 1];
        let resolved: Vec<ResolvedArtifact> = sizes
            .iter()
            .map(|&size| ResolvedArtifact {
                view: None,
                members: ResolvedMembers::Inline(vec![0; size as usize]),
                contents: Vec::new(),
                attached_to: None,
                parent_keys: Vec::new(),
                shape: None,
            })
            .collect();
        let artifacts: Vec<(&str, usize)> = (0..sizes.len()).map(|i| ("k", i)).collect();
        let table = spill::MemberTable::empty(sizes.len());
        assert_eq!(
            publication_batches(&artifacts, &resolved, &table, 6, false),
            vec![2, 3, 4, 5],
            "two artifacts of three fill a batch of six; the artifact of ten goes alone"
        );
        assert_eq!(
            publication_batches(&artifacts, &resolved, &table, 6, true),
            vec![5],
            "a layer whose edges run within a level is published whole"
        );
        assert_eq!(
            publication_batches(&artifacts, &resolved, &table, 1, false),
            vec![1, 2, 3, 4, 5],
            "a batch always takes an artifact, whatever it costs"
        );
    }

    /// The publication batch is the budget's share divided by what an entry costs, and never zero.
    #[test]
    fn the_publication_batch_is_a_share_of_the_budget() {
        assert_eq!(
            publication_batch_entries(24 << 30),
            (24u64 << 30) / PUBLICATION_BUDGET_SHARE / PUBLICATION_BYTES_PER_ENTRY
        );
        assert_eq!(publication_batch_entries(0), 1);
    }
}
