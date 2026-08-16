//! Annotation layers, and the artifacts in them, declared as **build inputs**.
//!
//! A layer registered on the control plane and a layer written by a build are the same object:
//! both end as a `RegisteredLayer` in `SEGMENTS-<n>.json`, and the engine seeds its registry from
//! that section before it replays a single WAL record. What this module adds is the route — a
//! declaration file and two Parquet files the build reads, so a bundle comes up with its layers
//! already there.
//!
//! **Why the build plane exists for this at all.** A 10⁷-artifact level is a build job for the same
//! reason `--attach-slice` is: volume that must not ride the trickle path, where every batch is an
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
//! in `(layer, level, stable_key)` order, and a stable key is **required** for a build-published
//! artifact — the caller's own name for it is the only address that survives a rebuild, and it is
//! what an edge into the layer names.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, ListArray, StringArray, UInt32Array, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use tessera_lifecycle::alloc::Allocator;
use tessera_lifecycle::membership::{
    ArtifactStore, IncomingArtifact, IncomingAttachment, IncomingVariation,
};
use tessera_lifecycle::LayerRegistry;
use tessera_store::manifest::{MembershipExtent, RecordExtent};
use tessera_types::layer::RegisteredLayer;
use tessera_types::layer::LayerDeclaration;
use tessera_types::EntityId;

use crate::error::{BuildError, Result};

/// The `--layers` file: a list of declarations, in registration order.
///
/// Registration order is the caller's and is not sorted: a layer must be registered after every
/// layer it declares in `depends_on`, which is the ordering constraint an edge's target-before-edge
/// rule imposes one level up (`annotation-representation.md` §5.0.4).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LayersFile {
    #[serde(default)]
    layer: Vec<LayerEntry>,
}

/// One layer as the file writes it, which is not quite the wire's declaration.
///
/// **Two differences, and both are about the gate.** TOML has no null, so a layer that is reachable
/// by everyone cannot be written as `label = null`; and the gate is exactly the field that must not
/// acquire a default, since the defaultable value — *no gate* — is the widest one there is (§4.3:
/// performance knobs default, disclosure controls do not). So the file states it either way round
/// and **states it explicitly**: `gate = "<term descriptor>"`, or `ungated = true`. Neither, or
/// both, is a refusal naming the choice rather than a bundle whose layer is public because a line
/// was mistyped.
///
/// Everything else is the declaration's own type, so a field's meaning here is the field's meaning
/// there: `artifacts_carry_own` and each supplied kind's `corpus_derived` keep having no default,
/// which is what the register watches them for (C27, C28).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerEntry {
    name: String,
    title: String,
    slices: Vec<String>,
    membership: tessera_types::layer::MembershipSource,
    /// The access label a viewer must satisfy to know this layer exists at all.
    #[serde(default)]
    gate: Option<String>,
    /// Reachable by every principal — the explicit form of *no gate*.
    #[serde(default)]
    ungated: bool,
    /// Whether each artifact carries its own access label. **No default** (C27).
    artifacts_carry_own: bool,
    #[serde(default)]
    visible_when: Option<tessera_types::layer::ExistenceCriterion>,
    #[serde(default)]
    hierarchy: Option<tessera_types::layer::Hierarchy>,
    #[serde(default)]
    content: tessera_types::layer::ContentDeclaration,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    levels: Vec<tessera_types::layer::LevelDeclaration>,
}

impl LayerEntry {
    fn into_declaration(self, path: &Path) -> Result<LayerDeclaration> {
        let label = match (self.gate, self.ungated) {
            (Some(label), false) => Some(label),
            (None, true) => None,
            (Some(_), true) => {
                return Err(BuildError::Invalid(format!(
                    "{}: layer {} declares both a gate and ungated = true",
                    path.display(),
                    self.name
                )))
            }
            (None, false) => {
                return Err(BuildError::Invalid(format!(
                    "{}: layer {} declares no gate; write the access label as gate = \"...\", or                      ungated = true to say every principal reaches it. There is no default,                      because the default would be the widest one",
                    path.display(),
                    self.name
                )))
            }
        };
        Ok(LayerDeclaration {
            name: self.name,
            title: self.title,
            slices: self.slices,
            membership: self.membership,
            access: tessera_types::layer::LayerAccess {
                label,
                artifacts_carry_own: self.artifacts_carry_own,
            },
            visible_when: self.visible_when,
            hierarchy: self.hierarchy.unwrap_or(tessera_types::layer::Hierarchy {
                kind: tessera_types::layer::HierarchyKind::Flat,
                prune_children: false,
            }),
            content: self.content,
            depends_on: self.depends_on,
            levels: self.levels,
        })
    }
}

/// One artifact as the build inputs describe it, before any id has been resolved.
#[derive(Debug, Default)]
struct PlannedArtifact {
    members: Vec<u64>,
    /// Indexed by variation, dense — a gap would silently renumber the caller's ranking.
    variations: Vec<PlannedVariation>,
    attached_to: Option<IncomingAttachment>,
}

#[derive(Debug, Default, Clone)]
struct PlannedVariation {
    values: Vec<String>,
    generated_from: Vec<u64>,
}

/// What the build reads: declarations, and the artifacts to publish into them.
pub struct LayerPlan {
    declarations: Vec<LayerDeclaration>,
    /// Keyed `(layer, level, stable_key)`, which is also the publication order — see the module
    /// doc on determinism.
    artifacts: BTreeMap<(String, u32, String), PlannedArtifact>,
}

/// What a build's layer pass produced, for the manifest and for the digest map.
pub struct PublishedLayers {
    pub layers: Vec<RegisteredLayer>,
    /// One past the lowest entity the layers and their artifacts claimed. **The mark that must
    /// reach the manifest**: the WAL carries the same one in its records and rotation reclaims
    /// those, so a mark that lived only there is lost at the first rotation and the next
    /// registration is handed ids a live layer already holds (decision 0074).
    pub low_water: u64,
    pub membership_extents: Vec<MembershipExtent>,
    pub artifact_record_extents: Vec<RecordExtent>,
    /// Every file written here, for `MANIFEST.files` — an undigested file is one a torn write
    /// cannot be attributed to.
    pub paths: Vec<PathBuf>,
}

impl Default for PublishedLayers {
    fn default() -> Self {
        PublishedLayers {
            layers: Vec::new(),
            low_water: tessera_types::layer::ROWLESS_CEILING,
            membership_extents: Vec::new(),
            artifact_record_extents: Vec::new(),
            paths: Vec::new(),
        }
    }
}

/// Read the declaration file and, if given, the two artifact files.
///
/// The artifact files are refused without a declaration file: an artifact names the layer it
/// belongs to, and a layer this build does not register is a name the manifest cannot carry.
pub fn read(
    layers: &Path,
    artifacts: Option<&Path>,
    members: Option<&Path>,
) -> Result<LayerPlan> {
    let text = std::fs::read_to_string(layers).map_err(|e| BuildError::io(layers, e))?;
    let file: LayersFile = toml::from_str(&text).map_err(|e| {
        BuildError::Invalid(format!("{}: {e}", layers.display()))
    })?;
    if file.layer.is_empty() {
        return Err(BuildError::Invalid(format!(
            "{}: declares no layer; omit --layers rather than passing an empty file, so a \
             mis-typed path is a refusal instead of a bundle with no layers in it",
            layers.display()
        )));
    }
    let mut plan = LayerPlan {
        declarations: file
            .layer
            .into_iter()
            .map(|entry| entry.into_declaration(layers))
            .collect::<Result<Vec<_>>>()?,
        artifacts: BTreeMap::new(),
    };
    if let Some(path) = artifacts {
        read_artifacts(path, &mut plan)?;
    }
    if let Some(path) = members {
        read_members(path, &mut plan)?;
    }
    Ok(plan)
}

/// One row per `(artifact, variation)`: the artifact's scalars, its content values, and the edge it
/// hangs from.
///
/// An artifact with no supplied content is one row with a null `variation` and no `values`; an
/// artifact with content is one row per variation. The attachment repeats on each of an artifact's
/// rows and must agree across them — a caller writing two different targets for one artifact has
/// written something nobody can act on, so it is a refusal rather than a last-row-wins.
fn read_artifacts(path: &Path, plan: &mut LayerPlan) -> Result<()> {
    for batch in batches(path)? {
        let batch = batch?;
        let layer = utf8(path, &batch, "layer")?;
        let level = optional_u32(path, &batch, "level")?;
        let key = utf8(path, &batch, "stable_key")?;
        let variation = optional_u32(path, &batch, "variation")?;
        let values = optional_string_list(path, &batch, "values")?;
        let target_layer = optional_utf8(path, &batch, "attached_layer")?;
        let target_level = optional_u32(path, &batch, "attached_level")?;
        let target_key = optional_utf8(path, &batch, "attached_key")?;

        for row in 0..batch.num_rows() {
            let address = address(path, layer, &level, key, row)?;
            let entry = plan.artifacts.entry(address.clone()).or_default();

            let attachment = match (
                target_layer.as_ref().and_then(|c| value_at(c, row)),
                target_key.as_ref().and_then(|c| value_at(c, row)),
            ) {
                (Some(layer), Some(stable_key)) => Some(IncomingAttachment {
                    layer,
                    level: target_level.as_ref().map_or(0, |c| number_at(c, row)),
                    stable_key,
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
            if attachment.is_some() {
                if entry.attached_to.is_some() && entry.attached_to != attachment {
                    return Err(BuildError::Invalid(format!(
                        "{}: artifact {} names two different attachment targets across its rows",
                        path.display(),
                        address.2
                    )));
                }
                entry.attached_to = attachment;
            }

            let Some(index) = variation.as_ref().and_then(|c| value_index(c, row)) else {
                continue;
            };
            let slot = variation_slot(entry, index);
            slot.values = values
                .as_ref()
                .map(|c| strings_at(c, row))
                .unwrap_or_default();
        }
    }
    Ok(())
}

/// One row per `(artifact, member)`: the memberships, and the generating sets beside them.
///
/// A null `variation` is the artifact's **membership**; `variation = k` is variation *k*'s
/// generating set — the documents a viewer must be able to see *entirely* before that description
/// is served to them. One file rather than two because the two are the same shape and the same
/// scale, and a build at 10⁷ artifacts reads whichever is larger the same way.
fn read_members(path: &Path, plan: &mut LayerPlan) -> Result<()> {
    for batch in batches(path)? {
        let batch = batch?;
        let layer = utf8(path, &batch, "layer")?;
        let level = optional_u32(path, &batch, "level")?;
        let key = utf8(path, &batch, "stable_key")?;
        let variation = optional_u32(path, &batch, "variation")?;
        let member = u64s(path, &batch, "member")?;

        for row in 0..batch.num_rows() {
            let address = address(path, layer, &level, key, row)?;
            let entry = plan.artifacts.entry(address.clone()).or_default();
            let source = member.value(row);
            match variation.as_ref().and_then(|c| value_index(c, row)) {
                None => entry.members.push(source),
                Some(index) => variation_slot(entry, index).generated_from.push(source),
            }
        }
    }
    Ok(())
}

/// The variation at `index`, growing the ranking to reach it.
///
/// **Dense, and a gap is refused.** A ranking is the caller's ordering and the service takes no
/// opinion on it (decision 0078), so a missing variation 1 under a present variation 2 would either
/// renumber the caller's ranking or publish an empty description; both are answers nobody wrote.
fn variation_slot(artifact: &mut PlannedArtifact, index: u32) -> &mut PlannedVariation {
    let index = index as usize;
    if index >= artifact.variations.len() {
        // Grown rather than refused here: rows arrive in file order, so a variation 2 seen before
        // a variation 1 is ordinary. A gap that is still a gap when the artifact is published is
        // caught there, over the whole ranking, by the empty-variation refusal.
        artifact
            .variations
            .resize_with(index + 1, PlannedVariation::default);
    }
    &mut artifact.variations[index]
}

/// Register every declaration, publish every artifact, and write the extents that carry them.
///
/// `resolve` maps a **source** entity id to the entity this build assigned it, and `high_water` is
/// the point region's mark — passed so the allocator refuses rather than letting the two regions
/// meet unnoticed.
pub fn publish(
    plan: &LayerPlan,
    resolve: &dyn Fn(u64) -> Option<u64>,
    high_water: u64,
    prefix_dir: &Path,
    partition: &str,
    slice: &str,
) -> Result<PublishedLayers> {
    let mut registry = LayerRegistry::new();
    let mut alloc = Allocator::new(high_water);
    let mut store = ArtifactStore::new();

    for declaration in &plan.declarations {
        let name = declaration.name.clone();
        // **A slice this build does not write is a refusal, not a layer that waits.** A layer
        // appears only in the slices it declares, so a mistyped slice name would produce a bundle
        // whose layer is registered, reachable, and serves nothing — indistinguishable, from every
        // client, from a layer whose artifacts all failed their existence criterion.
        if let Some(unknown) = declaration.slices.iter().find(|s| s.as_str() != slice) {
            return Err(BuildError::Invalid(format!(
                "layer {name} declares slice {unknown}, and this build writes slice {slice}; a                  layer in a slice that does not exist is registered, reachable and empty, which no                  client can tell from one whose artifacts were all withheld"
            )));
        }
        let record = registry
            .prepare_create(declaration.clone(), &mut alloc)
            .map_err(|e| BuildError::Invalid(format!("layer {name}: {e}")))?;
        registry.apply(&record);
    }

    // Grouped by `(layer, level)` and published in key order, so a level's ordinals — and therefore
    // its entities — are a function of the artifacts, never of the file's row order.
    let mut batched: BTreeMap<(&str, u32), Vec<(&str, &PlannedArtifact)>> = BTreeMap::new();
    for ((layer, level, key), artifact) in &plan.artifacts {
        batched
            .entry((layer.as_str(), *level))
            .or_default()
            .push((key.as_str(), artifact));
    }

    for ((layer, level), artifacts) in batched {
        let mut incoming = Vec::with_capacity(artifacts.len());
        for (key, artifact) in artifacts {
            incoming.push(resolved(layer, level, key, artifact, resolve)?);
        }
        let record = registry
            .prepare_publish(layer, level, &incoming, &store, &mut alloc)
            .map_err(|e| BuildError::Invalid(format!("publishing into {layer}: {e}")))?;
        registry.apply(&record);
        let refused = store.apply(&record, 0);
        if refused > 0 {
            // Unreachable: the memberships were serialised from bitmaps two calls ago. It is a
            // refusal rather than an assertion because the alternative is a level published with
            // artifacts silently missing, which serves as *absent* with nothing reporting a fault.
            return Err(BuildError::Invalid(format!(
                "{refused} membership(s) of {layer} did not survive their own encoding"
            )));
        }
    }

    let (layers, _tombstones) = registry.snapshot();
    let mut published = PublishedLayers {
        layers,
        low_water: alloc.low_water(),
        ..PublishedLayers::default()
    };
    write_membership_extents(&store, prefix_dir, partition, &mut published)?;
    write_content_extent(&store, prefix_dir, partition, &mut published)?;
    Ok(published)
}

/// One planned artifact with every source id resolved to the entity this build assigned it.
fn resolved(
    layer: &str,
    level: u32,
    key: &str,
    artifact: &PlannedArtifact,
    resolve: &dyn Fn(u64) -> Option<u64>,
) -> Result<IncomingArtifact> {
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

    let members = entities(&artifact.members, "membership")?;
    let mut variations = Vec::with_capacity(artifact.variations.len());
    for (index, variation) in artifact.variations.iter().enumerate() {
        if variation.values.is_empty() && variation.generated_from.is_empty() {
            return Err(BuildError::Invalid(format!(
                "{layer} level {level} artifact {key}: variation {index} is empty, so the ranking \
                 above it names a description that was never supplied"
            )));
        }
        variations.push(IncomingVariation::new(
            variation.values.clone(),
            entities(&variation.generated_from, &format!("variation {index}"))?,
        ));
    }

    Ok(match artifact.attached_to.clone() {
        None => IncomingArtifact::with_content(Some(key.to_string()), members, variations),
        Some(attached_to) => {
            IncomingArtifact::attached(Some(key.to_string()), members, variations, attached_to)
        }
    })
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
    let dir = prefix_dir.join("partitions").join(partition).join("members");
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
// Narrow helpers rather than a general reader: these two files have six and five columns, every
// one of which is named here, so a mistyped column is a refusal naming the column rather than a
// silent absence.

fn batches(
    path: &Path,
) -> Result<impl Iterator<Item = Result<arrow::record_batch::RecordBatch>> + '_> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| BuildError::parquet(path, e))?
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    Ok(reader.map(move |batch| batch.map_err(|e| BuildError::arrow(path, e))))
}

fn column<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a std::sync::Arc<dyn Array>> {
    batch.column_by_name(name).ok_or_else(|| {
        BuildError::Invalid(format!("{}: no column named {name}", path.display()))
    })
}

fn typed<'a, T: 'static>(path: &Path, array: &'a std::sync::Arc<dyn Array>, name: &str) -> Result<&'a T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        BuildError::Invalid(format!(
            "{}: column {name} is {:?}, which this reader cannot take",
            path.display(),
            array.data_type()
        ))
    })
}

fn utf8<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a StringArray> {
    typed(path, column(path, batch, name)?, name)
}

fn optional_utf8<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<&'a StringArray>> {
    match batch.column_by_name(name) {
        None => Ok(None),
        Some(array) => typed(path, array, name).map(Some),
    }
}

fn u64s<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a UInt64Array> {
    typed(path, column(path, batch, name)?, name)
}

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

fn optional_string_list<'a>(
    path: &Path,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<&'a ListArray>> {
    match batch.column_by_name(name) {
        None => Ok(None),
        Some(array) => typed(path, array, name).map(Some),
    }
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

fn strings_at(column: &ListArray, row: usize) -> Vec<String> {
    if column.is_null(row) {
        return Vec::new();
    }
    let values = column.value(row);
    match values.as_any().downcast_ref::<StringArray>() {
        Some(strings) => (0..strings.len()).map(|i| strings.value(i).to_string()).collect(),
        None => Vec::new(),
    }
}

/// One row's `(layer, level, stable_key)`.
///
/// **A stable key is required**, and its absence is a refusal rather than a generated name: an
/// artifact published without one can be named by no edge and matched by no later build, and the
/// caller is the only party who knows what it should be called.
fn address(
    path: &Path,
    layer: &StringArray,
    level: &Option<&UInt32Array>,
    key: &StringArray,
    row: usize,
) -> Result<(String, u32, String)> {
    if layer.is_null(row) || key.is_null(row) {
        return Err(BuildError::Invalid(format!(
            "{}: row {row} names no layer or no stable_key; a build-published artifact carries \
             the caller's own name for it, which is what an edge into it names",
            path.display()
        )));
    }
    Ok((
        layer.value(row).to_string(),
        level.map_or(0, |c| number_at(c, row)),
        key.value(row).to_string(),
    ))
}
