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
//! ## The declarations are not read here
//!
//! [`crate::config`] parses them, out of the one document that also carries the attributes, the
//! vocabularies and the views. What is left in this module is the *data* path: each layer's own
//! artifacts and members, the publication order, and the hierarchy checks that need every artifact
//! in hand.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{
    Array, FixedSizeListArray, Int16Array, Int32Array, Int64Array, Int8Array,
    ListArray, StringArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
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
use tessera_store::derived::authored_shape_input;

/// One artifact as the build inputs describe it, before any id has been resolved.
#[derive(Debug, Default)]
struct PlannedArtifact {
    membership: PlannedMembership,
    /// Indexed by rank, dense — a gap would silently renumber the caller's ranking.
    contents: Vec<PlannedContent>,
    attached_to: Option<IncomingAttachment>,
    /// Parent artifact in a hierarchy, named by the parent's own key.
    parent_key: Option<String>,
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
#[derive(Debug, Clone)]
enum PlannedMembership {
    /// Rows from a `[layer.members]` source, accumulated — and the empty membership of an artifact
    /// no source named.
    Rows(Vec<u64>),
    /// The artifact row's own `members` list.
    Included(Vec<u64>),
    /// The artifact row's `excluding` list: the entities the membership leaves out.
    Excluded(Vec<u64>),
}

impl Default for PlannedMembership {
    fn default() -> Self {
        PlannedMembership::Rows(Vec::new())
    }
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
    members: Vec<EntityId>,
    contents: Vec<IncomingContent>,
    attached_to: Option<IncomingAttachment>,
    parent_key: Option<String>,
    shape: Option<ArtifactShapes>,
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

/// What a build's layer pass produced, for the manifest and for the digest map.
pub struct PublishedLayers {
    pub layers: Vec<RegisteredLayer>,
    /// Edges whose child escapes its parent's membership — reported, never acted on.
    pub containment_violations: Vec<ContainmentViolation>,
    /// Per-parent coverage: how much of each split its children hold between them.
    pub split_coverage: Vec<SplitCoverage>,
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
    /// of the build rather than being read back off the extents this just wrote, which would parse
    /// every membership a second time to reach a structure that is already in hand.
    ///
    /// It is carried across the build's residency peak, and that is a real cost stated rather than
    /// hidden: the memberships are one bitmap per artifact over the corpus, tens of megabytes at
    /// the campaign's 10⁵ artifacts against a peak measured in gigabytes.
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
            containment_violations: Vec::new(),
            split_coverage: Vec::new(),
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
pub fn read(
    declarations: &[LayerDeclaration],
    inputs: &[LayerSources],
    projection: tessera_spatial::Projection,
    extent: &tessera_spatial::Bounds,
    max_shape_vertices: u64,
) -> Result<LayerPlan> {
    let mut plan = LayerPlan {
        bodies: Vec::new(),
        addresses: Vec::new(),
        declarations: declarations.to_vec(),
        shape_reports: Vec::new(),
        artifacts: BTreeMap::new(),
        unclustered: Vec::new(),
        minted: BTreeMap::new(),
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
        let mut shapes = shape_declared(declaration).map(|kind| {
            ShapeReader::new(
                &input.name,
                kind,
                ShapeContext {
                    extent: *extent,
                    projection,
                    views: declaration.views.clone(),
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
                &mut plan,
                shapes.as_mut(),
            )?,
            Some(ArtifactSource::Inline(rows)) => {
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
                .filter_map(|((_, _, key), index)| {
                    plan.bodies[*index]
                        .parent_key
                        .clone()
                        .map(|parent| (key.clone(), parent))
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
                &format!("{} (authored `{}` content '{content_name}')", input.name, kind.as_str()),
                kind,
                ShapeContext {
                    extent: *extent,
                    projection,
                    views: declaration.views.clone(),
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
fn read_artifacts(
    layer: &str,
    path: &Path,
    fields: &Fields,
    enumerated: bool,
    plan: &mut LayerPlan,
    mut shapes: Option<&mut ShapeReader>,
) -> Result<()> {
    for batch in batches(path)? {
        let batch = batch?;
        let key = key_column(path, &batch, fields, "key")?;
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
        let parent = optional_utf8(path, &batch, fields, "parent")?;

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
            let index = plan.intern(address.clone());
            plan.bodies[index] = PlannedArtifact {
                    membership,
                    contents: match contents.as_ref() {
                        None => Vec::new(),
                        Some(column) => ranked_at(path, column, row, &address.2)?,
                    },
                    attached_to: attachment,
                    parent_key: parent.as_ref().and_then(|c| value_at(c, row)),
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
                parent_key: row.parent.clone(),
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
/// and `tiered`, a lineage for `nested`. The entries name the artifacts the point belongs to,
/// exactly as a scalar names the one, and `tiered` and `nested` read their **edges** from the
/// adjacency the list itself carries.
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
    // The edges a list column declared, child address → parent key. **One entry per child, not one
    // per row**: a cluster of a hundred thousand points states its parent a hundred thousand times,
    // and the second statement onward is a comparison rather than an insertion. Applied once the
    // whole source has been read, so a conflict is found wherever in the file it sits.
    let mut lineage: BTreeMap<usize, usize> = BTreeMap::new();
    // Reused across rows rather than allocated per point: one slot per position in the row's list,
    // `None` where the entry named no artifact.
    let mut entries: Vec<Option<usize>> = Vec::new();
    let mut said_level_is_ignored = false;
    for batch in batches(path)? {
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
    let key = plan.addresses[index].2.clone();
    let entry = &mut plan.bodies[index];
    match rank {
        None => match &mut entry.membership {
            PlannedMembership::Rows(members) => members.push(source),
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
                    key
                )))
            }
        },
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

/// The edges one row's list declares, folded into the map of what each child's parent is.
///
/// The adjacency itself is [`parent_edges`]'s — the wire reads the same rule off the same function
/// — and what is added here is the conflict: **one entry per child, not one per row**, so a cluster
/// of a hundred thousand points states its parent a hundred thousand times and the second statement
/// onward is a comparison rather than an insertion.
fn record_lineage(
    entries: &[Option<usize>],
    plan: &LayerPlan,
    lineage: &mut BTreeMap<usize, usize>,
    path: &Path,
) -> Result<()> {
    for (parent, child) in parent_edges(entries) {
        match lineage.get(child) {
            Some(first) if first != parent => {
                return Err(two_parents(
                    path,
                    plan.address_of(*child),
                    &plan.address_of(*first).2,
                    &plan.address_of(*parent).2,
                ))
            }
            Some(_) => {}
            None => {
                lineage.insert(*child, *parent);
            }
        }
    }
    Ok(())
}

/// Hang every child the column named under the parent it named.
///
/// **A parent already on the artifact row must be the same one**: a `parent` column and a lineage
/// column are two spellings of one edge, and an artifact holding a different parent in each is the
/// same conflict as two points disagreeing.
fn apply_lineage(
    plan: &mut LayerPlan,
    lineage: BTreeMap<usize, usize>,
    path: &Path,
) -> Result<()> {
    // **Applied in address order, not arena order.** The conflict below is a refusal, and which of
    // several a corpus carries is reported must not depend on the order keys happened to be met —
    // it is the order they sort in, which is what it has always been. One sort of at most one entry
    // per child, against one probe per member row.
    let mut in_order: Vec<(usize, usize)> = lineage.into_iter().collect();
    in_order.sort_by(|a, b| plan.address_of(a.0).cmp(plan.address_of(b.0)));
    for (child, parent) in in_order {
        let parent_key = plan.address_of(parent).2.clone();
        let address = plan.address_of(child).clone();
        let artifact = &mut plan.bodies[child];
        match &artifact.parent_key {
            Some(declared) if *declared != parent_key => {
                return Err(two_parents(path, &address, declared, &parent_key))
            }
            _ => artifact.parent_key = Some(parent_key),
        }
    }
    Ok(())
}

/// **A child naming two different parents is refused** (`artifacts-from-points.md` §4). The data is
/// not the tree the layer declared: there is no correct output, and choosing a parent would publish
/// a hierarchy the caller did not write.
fn two_parents(path: &Path, child: &Address, first: &str, second: &str) -> BuildError {
    BuildError::Invalid(format!(
        "{}: {} in level {} of {} is named as a child of both {first} and {second}. A list key \
         column declares the edges, so two rows naming different parents for one artifact are two \
         hierarchies — and which of them was published would be the file's row order rather than \
         anything the caller wrote",
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
    plan: &LayerPlan,
    resolve: &(dyn Fn(u64) -> Option<u64> + Sync),
    high_water: u64,
    prefix_dir: &Path,
    partition: &str,
    view: &str,
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
        if let Some(unknown) = declaration.views.iter().find(|s| s.as_str() != view) {
            return Err(BuildError::Invalid(format!(
                "layer {name} declares view {unknown}, and this build writes view {view}. A layer \
                 in a view this build does not write is registered, reachable and empty, which no \
                 client can tell from one whose artifacts were all withheld"
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
    // **Resolved in parallel, collected in key order.** Each artifact's resolution reads only its
    // own planned body and the `resolve` closure, which is a lookup into two immutable arrays — so
    // there is no cross-artifact state and nothing to order. The *output* order is the `BTreeMap`'s
    // and therefore the keys', not the scheduler's, which is what keeps ordinals a function of the
    // artifacts under I9.
    //
    // It is the one phase of this stage worth parallelising first: it is a pure map, and at the
    // Overture rung it is 3×10⁸ member entries through the source-id lookup.
    let resolved: BTreeMap<Address, ResolvedArtifact> = plan
        .artifacts
        .par_iter()
        .map(|((layer, level, key), index)| {
            resolve_artifact(
                layer,
                *level,
                key,
                &plan.bodies[*index],
                resolve,
                high_water,
            )
            .map(|artifact| ((layer.clone(), *level, key.clone()), artifact))
        })
        .collect::<Result<_>>()?;

    // **The hierarchy checks run before a single entity is allocated**, which is both the cheaper
    // and the more useful order: they read `declarations` and `resolved` and touch neither the
    // registry nor the store, and a structural fault in the edges is worth refusing before the
    // publication that assigns permanent ids rather than after it. It also lets `resolved` be
    // *consumed* below — the memberships are the largest thing this stage holds and nothing else
    // needs them whole.
    let (violations, coverage) = verify_hierarchies(&plan.declarations, &resolved)?;

    // Grouped by `(layer, level)`, each level's artifacts in key order — so a level's
    // ordinals, and therefore its entities, are a function of the artifacts and never of the file's
    // row order.
    //
    // **Owned, so a level's members are freed as it publishes.** Borrowing held every level's
    // `Vec<EntityId>` alive until the last level was published, so the peak carried the whole
    // corpus's memberships twice over — once as entity vectors and once as the Roaring bitmaps
    // built from them. Consuming `resolved` here means each level's vectors drop at the end of the
    // iteration that turned them into bitmaps.
    let mut batched: BTreeMap<(String, u32), Vec<(String, ResolvedArtifact)>> = BTreeMap::new();
    for ((layer, level, key), artifact) in resolved {
        batched
            .entry((layer, level))
            .or_default()
            .push((key, artifact));
    }

    // **Published in declaration order, which is the order that honours `depends_on`.** An
    // attachment resolves against what is already published, so a label layer must follow the layer
    // it attaches into — and iterating the map instead would publish in alphabetical order, making
    // an operator's file work or fail on how their layers happen to sort.
    let mut order: Vec<(String, u32)> = Vec::with_capacity(batched.len());
    for declaration in &plan.declarations {
        let name = declaration.name.as_str();
        order.extend(
            batched
                .keys()
                .filter(|(layer, _)| layer == name)
                .cloned(),
        );
    }

    for address in order {
        let (layer, level) = (address.0.as_str(), address.1);
        let artifacts = batched
            .remove(&address)
            .expect("every address came from the map a statement ago");
        let mut incoming = Vec::with_capacity(artifacts.len());
        for (key, artifact) in &artifacts {
            incoming.push(incoming_artifact(key, artifact));
        }
        // The entity vectors are done with the moment they are bitmaps; holding them through the
        // publication is what made the peak twice what it needed to be.
        drop(artifacts);
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
        // The record carries its own copy of every membership, so the bitmaps this built are dead
        // the moment `prepare_publish` returns — a level's worth of them, held to the end of the
        // loop for nothing.
        drop(incoming);
        registry.apply(&record);
        let refused = store.apply(&record, 0);
        if refused > 0 {
            // **Reached once, and not by a fault in the encoding.** The 5×10⁷ tier of
            // `probes/2026-08-22-artifact-serving-e2e/` stopped here on one membership of
            // `generator/treed`; the bytes carried exactly what the container held, and what the
            // decoder's validation rejected was a container whose array was already out of order
            // when it was serialised. The message says that rather than blaming the format,
            // because an operator told the encoding failed will look at the wrong half.
            //
            // A refusal rather than an assertion because the alternative is a level published with
            // artifacts silently missing, which serves as *absent* with nothing reporting a fault.
            return Err(BuildError::Invalid(format!(
                "{refused} membership(s) of {layer} were not well-formed bitmaps when this build \
                 encoded them — the bytes decode to nothing, so the level is refused rather than \
                 published with those artifacts absent"
            )));
        }
    }

    let (layers, _tombstones) = registry.snapshot();
    let mut published = PublishedLayers {
        layers,
        containment_violations: violations,
        split_coverage: coverage,
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
    write_membership_extents(&store, prefix_dir, partition, &mut published)?;
    write_content_extent(&store, prefix_dir, partition, &mut published)?;
    published.store = store;
    Ok(published)
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

fn verify_hierarchies(
    declarations: &[LayerDeclaration],
    artifacts: &BTreeMap<Address, ResolvedArtifact>,
) -> Result<(Vec<ContainmentViolation>, Vec<SplitCoverage>)> {
    // Which parent has claimed each child, so a second claim is a refusal rather than a silent
    // reparenting: a child with two parents has two lineages, and which one a cut walks would
    // depend on iteration order.
    let mut claimed: BTreeMap<(&str, u32, &str), &str> = BTreeMap::new();
    // Children grouped under their parent, so containment and coverage are one pass over each
    // parent's membership rather than one per edge. **Each child by its full address**, because an
    // tiered layer's child sits at a different level from its parent and a bare key would
    // then be looked up in the wrong one.
    let mut children_of: BTreeMap<Address, Vec<Address>> = BTreeMap::new();

    // Which shape each layer's edges have, from its declaration and never from the edges
    // themselves. A layer that declares no lineage may carry none; a nested layer's edges stay
    // within a level; a tiered layer's run from a coarser level to a finer one, and it is
    // the levels that carry the resolution rather than the edges.
    let kind_of: BTreeMap<&str, tessera_types::layer::HierarchyKind> = declarations
        .iter()
        .map(|d| (d.name.as_str(), d.hierarchy.kind))
        .collect();

    for ((layer, level, key), artifact) in artifacts {
        let Some(parent_key) = artifact.parent_key.as_deref() else {
            continue;
        };
        let kind = kind_of
            .get(layer.as_str())
            .copied()
            .unwrap_or(tessera_types::layer::HierarchyKind::Flat);
        let cross_level = matches!(kind, tessera_types::layer::HierarchyKind::Tiered);
        if !cross_level && !matches!(kind, tessera_types::layer::HierarchyKind::Nested) {
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
                if artifacts.contains_key(&candidate) {
                    if found.is_some() {
                        return Err(BuildError::Invalid(format!(
                            "{layer} artifact {key} names parent {parent_key}, which exists in \
                             more than one coarser level; which level the edge meant would depend \
                             on the search order, so it is refused rather than resolved"
                        )));
                    }
                    found = Some(candidate);
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
            let address = (layer.clone(), *level, parent_key.to_string());
            if !artifacts.contains_key(&address) {
                return Err(BuildError::Invalid(format!(
                    "{layer} level {level} artifact {key} names parent {parent_key}, which this \
                     level does not declare — a nested layer's edges relate two artifacts of one \
                     level, and a parent that does not exist would leave the child a root of a \
                     tree nobody wrote"
                )));
            }
            address
        };
        // **Only a within-level edge can name itself.** A key is unique per `(layer,
        // level)`, so a levelled taxonomy legitimately carries the same key at two levels — an
        // arXiv archive with no subclass is `hep-ph` at both, and the level-1 artifact's parent is
        // the level-0 one of the same name.
        if !cross_level && parent_key == key {
            return Err(BuildError::Invalid(format!(
                "{layer} level {level} artifact {key} names itself as its parent"
            )));
        }
        if let Some(first) = claimed.insert((layer, *level, key), parent_key) {
            return Err(BuildError::Invalid(format!(
                "{layer} level {level} artifact {key} is claimed by both {first} and {parent_key}; \
                 a child has one lineage or the cut that walks it depends on iteration order"
            )));
        }
        children_of
            .entry(parent_address)
            .or_default()
            .push((layer.clone(), *level, key.clone()));
    }

    let mut violations = Vec::new();
    let mut coverage = Vec::new();
    for (address, children) in &children_of {
        let (layer, level, parent_key) = address;
        let parent = &artifacts[address];
        // **The parent's distinct members, in order** — `resolve_artifact` sorted them, so this is
        // a run-skip rather than a sort. It replaces a `HashSet<u64>` per parent, which for a
        // country-level division holding 2×10⁷ points was a ~300 MB table built and torn down, with
        // a second one beside it for what the children covered.
        let mut held: Vec<u64> = parent.members.iter().map(|e| e.raw()).collect();
        held.dedup();
        // One bit per distinct member, so `covered.len()` becomes a popcount: 2.5 MB where the
        // second `HashSet` was 300 MB, and the counts it feeds are identical by construction.
        let mut covered = vec![0u64; held.len().div_ceil(64)];
        let mut covered_count = 0u64;

        for child_address in children {
            let child = &artifacts[child_address];
            let (_, child_level, child_key) = child_address;
            let mut escaping = 0u64;
            // **Galloping from a cursor**, both sides being sorted: a child whose members sit in
            // one region of the parent's finds them in a few probes each rather than a full
            // binary search, and the walk is cache-resident where the hash table was not.
            let mut cursor = 0usize;
            for member in child.members.iter().map(|e| e.raw()) {
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
                    // The **child's** level, which is the one an operator needs to find it; for a
                    // nested layer it is the parent's too, and for a tiered one it is not.
                    level: *child_level,
                    child: child_key.clone(),
                    parent: parent_key.clone(),
                    escaping_members: escaping,
                });
            }
        }

        coverage.push(SplitCoverage {
            layer: layer.clone(),
            level: *level,
            parent: parent_key.clone(),
            children: children.len() as u32,
            members: held.len() as u64,
            stray_members: held.len() as u64 - covered_count,
        });
    }

    detect_cycles(artifacts, &kind_of)?;
    Ok((violations, coverage))
}

/// Refuse a hierarchy holding a cycle, which is not a tree and has no root to descend from.
///
/// Walks each artifact's ancestry to the root, bounded by the level's own artifact count — a chain
/// longer than that has revisited a node, whatever the shape of the loop.
///
/// **Only a nested layer can hold one, and only its edges are walked.** A tiered layer's
/// edges each step to a strictly coarser level, and the levels are finite and bounded below by
/// zero, so a cycle is not expressible there.
///
/// **Skipping such a layer is required, not an optimisation.** Its keys are unique per level and
/// may legitimately repeat across them — an arXiv archive with no subclass is `hep-ph` at both —
/// so the same-level walk below would follow `hep-ph` at level 1 back to itself and report the
/// taxonomy as a cycle. That is exactly what it did before this guard existed, and the demo corpus
/// is what found it.
fn detect_cycles(
    artifacts: &BTreeMap<Address, ResolvedArtifact>,
    kind_of: &BTreeMap<&str, tessera_types::layer::HierarchyKind>,
) -> Result<()> {
    // One `key → parent` map per nested level, in key order. The walk below reads nothing else, so
    // building this once is what lets it borrow rather than clone an `Address` at every step — the
    // per-artifact walk allocated three `String`s per step and ran a step per ancestor.
    //
    // A `BTreeMap` at both levels, because the order artifacts are visited in is the order this
    // reports a cycle in, and that order must stay `artifacts.keys()`'s.
    let mut levels: BTreeMap<(&str, u32), BTreeMap<&str, Option<&str>>> = BTreeMap::new();
    for ((layer, level, key), artifact) in artifacts {
        if !matches!(
            kind_of.get(layer.as_str()),
            Some(tessera_types::layer::HierarchyKind::Nested)
        ) {
            continue;
        }
        levels
            .entry((layer.as_str(), *level))
            .or_default()
            .insert(key.as_str(), artifact.parent_key.as_deref());
    }

    // **One visit per artifact, not one walk per artifact.** Each node is coloured once it is known
    // to reach a root, so a chain already proved good is left the moment it is re-entered — and a
    // node met while still on the current chain *is* the cycle, which is what the count bound was
    // standing in for. The bound it replaces was derived by scanning the whole map per artifact,
    // O(A²), and a `nested` layer puts every artifact at level 0 (`configuration.md`, the four
    // hierarchy kinds): 600,000 artifacts at the Overture rung, 3.6×10¹¹ key visits, and the whole
    // of that build's layers stage. It also removes a latent hang — a corpus that genuinely held a
    // cycle ran the bound's full length for every artifact whose lineage reached it.
    //
    // **The artifact reported is the same one**: the walks start in key order and the first start
    // whose lineage reaches a cycle is the first artifact the counted walk would have failed on.
    for ((layer, level), parents) in &levels {
        // 0 unvisited · 1 on the chain being walked · 2 known to reach a root
        let mut state: std::collections::HashMap<&str, u8> =
            std::collections::HashMap::with_capacity(parents.len());
        for start in parents.keys() {
            let mut chain: Vec<&str> = Vec::new();
            let mut node = *start;
            loop {
                match state.get(node).copied().unwrap_or(0) {
                    2 => break,
                    1 => {
                        return Err(BuildError::Invalid(format!(
                            "{layer} level {level}: the lineage above {start} does not reach a \
                             root within the level's own artifact count, so the edges hold a \
                             cycle — a tree has a root to descend a cut from and a cycle has none"
                        )))
                    }
                    _ => {}
                }
                state.insert(node, 1);
                chain.push(node);
                match parents.get(node).copied().flatten() {
                    // A root, or a parent this level does not hold — the chain ends, and what sits
                    // above a key this level never declared is not this level's to call a cycle.
                    None => break,
                    Some(parent) if !parents.contains_key(parent) => break,
                    Some(parent) => node = parent,
                }
            }
            for node in chain {
                state.insert(node, 2);
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
    artifact: &PlannedArtifact,
    resolve: &(dyn Fn(u64) -> Option<u64> + Sync),
    high_water: u64,
) -> Result<ResolvedArtifact> {
    let entities = |ids: &[u64], what: &str| -> Result<Vec<EntityId>> {
        ids.iter()
            .map(|&source| {
                resolve(source).map(EntityId::new).ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "{layer} level {level} artifact {key}: {what} names entity {source}, which \
                         this build did not assign — the batch is refused rather than published \
                         without it, a dropped member moving both the count a viewer is shown and \
                         the size a proportional criterion divides by"
                    ))
                })
            })
            .collect()
    };

    let members = match &artifact.membership {
        PlannedMembership::Rows(ids) => entities(ids, "membership")?,
        PlannedMembership::Included(ids) => entities(ids, "membership")?,
        // **An excluded id this build did not assign refuses the build**, on the same rule an
        // unknown member does and for a sharper reason: an exclusion that resolves to nothing
        // silently *widens* the membership by the item it was meant to keep out.
        PlannedMembership::Excluded(ids) => {
            let excluded: std::collections::HashSet<u64> = entities(ids, "exclusion")?
                .into_iter()
                .map(|e| e.raw())
                .collect();
            (0..high_water)
                .filter(|entity| !excluded.contains(entity))
                .map(EntityId::new)
                .collect()
        }
    };

    // **Sorted here, once, and not deduped.** Two readers want it in order — the containment pass
    // below, which walks parent and child together instead of hashing a set per parent, and
    // `bitmap_of_entities`, which sorts before its bulk add. Deduping here would be wrong: a
    // containment violation counts member *entries* that escape, duplicates included, and that is
    // the number an operator is given.
    let mut members = members;
    members.sort_unstable_by_key(|e| e.raw());

    let mut contents = Vec::with_capacity(artifact.contents.len());
    for (rank, content) in artifact.contents.iter().enumerate() {
        if content.values.is_empty() && content.generated_from.is_empty() {
            return Err(BuildError::Invalid(format!(
                "{layer} level {level} artifact {key}: contents[{rank}] is empty, so the ranking \
                 above it names a description that was never supplied"
            )));
        }
        contents.push(IncomingContent::new(
            content.values.clone(),
            entities(&content.generated_from, &format!("contents[{rank}]"))?,
        ));
    }

    Ok(ResolvedArtifact {
        members,
        contents,
        attached_to: artifact.attached_to.clone(),
        parent_key: artifact.parent_key.clone(),
        shape: artifact.shape.clone(),
    })
}

/// The same artifact as the registry takes it. A membership is a list of entities by this point,
/// so there is nothing here to decide.
fn incoming_artifact(key: &str, artifact: &ResolvedArtifact) -> IncomingArtifact {
    let mut result = match artifact.attached_to.clone() {
        None => IncomingArtifact::with_content(
            Some(key.to_string()),
            artifact.members.iter().copied(),
            artifact.contents.clone(),
        ),
        Some(attached_to) => IncomingArtifact::attached(
            Some(key.to_string()),
            artifact.members.iter().copied(),
            artifact.contents.clone(),
            attached_to,
        ),
    };
    result.parent_key = artifact.parent_key.clone();
    result.shape = artifact.shape.clone();
    result
}

/// Pack every level's memberships into one extent and fsync it — the same format, one file per
/// level, that a control-plane publication writes.
fn write_membership_extents(
    store: &ArtifactStore,
    prefix_dir: &Path,
    partition: &str,
    published: &mut PublishedLayers,
) -> Result<()> {
    let (ready, skipped) = store.unpublished();
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
    for (index, (layer, level, ordinal_lo, blobs)) in ready.into_iter().enumerate() {
        // **The layer name never reaches the filename.** It is path-shaped — `clusters/a` — so a
        // name-derived path would escape the directory, or collide after escaping.
        let name = format!("members-000000-{index:03}.tsmb");
        let count = blobs.len() as u32;
        let bytes = tessera_store::membership::pack(ordinal_lo, &blobs);
        let path = dir.join(&name);
        tessera_store::write_and_fsync(&path, &bytes).map_err(BuildError::Store)?;
        published.paths.push(path);
        published.membership_extents.push(MembershipExtent {
            path: format!("partitions/{partition}/members/{name}"),
            layer,
            level,
            ordinal_lo,
            count,
        });
    }
    tessera_store::fsync_dir(&dir).map_err(BuildError::Store)?;
    Ok(())
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

enum ListShape<'a> {
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
    by_text: BTreeMap<u32, std::collections::HashMap<Box<str>, usize>>,
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
            }],
            "withdraw_on_member_deletion": true
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
        let plan = read(
            &declarations,
            &sources,
            projection,
            &extent,
            DEFAULT_MAX_SHAPE_VERTICES,
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

    /// The drawing layer on its own, so that a refusal below is the authored path's own and not
    /// the membership shape beside it reaching the same check first.
    fn draws_only(wkt: &str, projection: Projection, extent: Bounds) -> BuildError {
        let (declarations, mut sources) = two_layers(Some("wgs84"));
        sources.retain(|s| s.name == "regions/draws");
        sources[0].artifacts = Some(ArtifactSource::Inline(vec![serde_json::from_value(
            serde_json::json!({ "key": "uk", "space": "wgs84", "contents": [[wkt]] }),
        )
        .expect("the fixture row is well-formed")]));
        read(
            &declarations,
            &sources,
            projection,
            &extent,
            DEFAULT_MAX_SHAPE_VERTICES,
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
}
