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

use arrow::array::{Array, ListArray, StringArray, UInt32Array, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use tessera_lifecycle::alloc::Allocator;
use tessera_lifecycle::membership::{
    ArtifactStore, IncomingArtifact, IncomingAttachment, IncomingContent,
};
use tessera_lifecycle::LayerRegistry;
use tessera_store::manifest::{MembershipExtent, RecordExtent};
use tessera_types::layer::RegisteredLayer;
use tessera_types::layer::LayerDeclaration;
use tessera_types::EntityId;

use crate::config::{ArtifactSource, Fields, InlineArtifact, LayerSources};
use crate::error::{BuildError, Result};

/// One artifact as the build inputs describe it, before any id has been resolved.
#[derive(Debug, Default)]
struct PlannedArtifact {
    membership: PlannedMembership,
    /// Indexed by rank, dense — a gap would silently renumber the caller's ranking.
    contents: Vec<PlannedContent>,
    attached_to: Option<IncomingAttachment>,
    /// Parent artifact in a hierarchy, named by the parent's own key.
    parent_key: Option<String>,
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
}

/// What the build reads: declarations, and the artifacts to publish into them.
pub struct LayerPlan {
    declarations: Vec<LayerDeclaration>,
    /// Keyed `(layer, level, key)`, which is also the publication order — see the module doc on
    /// determinism.
    artifacts: BTreeMap<(String, u32, String), PlannedArtifact>,
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
    pub artifact_record_extents: Vec<RecordExtent>,
    /// Every file written here, for `MANIFEST.files` — an undigested file is one a torn write
    /// cannot be attributed to.
    pub paths: Vec<PathBuf>,
}

impl Default for PublishedLayers {
    fn default() -> Self {
        PublishedLayers {
            layers: Vec::new(),
            containment_violations: Vec::new(),
            split_coverage: Vec::new(),
            low_water: tessera_types::layer::ROWLESS_CEILING,
            membership_extents: Vec::new(),
            artifact_record_extents: Vec::new(),
            paths: Vec::new(),
        }
    }
}

/// Take the config's layer declarations and read each layer's own artifacts against them.
///
/// **One source per layer, so no row names the layer it belongs to.** The layer is the input's
/// own, which is what retires the discriminator column: there is no second layer's rows in the
/// file to tell apart, no filter to configure, and no way for a layer to ingest another's rows
/// (`annotation-write-cycle.md` §6.1).
pub fn read(declarations: &[LayerDeclaration], inputs: &[LayerSources]) -> Result<LayerPlan> {
    let mut plan = LayerPlan {
        declarations: declarations.to_vec(),
        artifacts: BTreeMap::new(),
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
        let enumerated = declaration.membership
            == tessera_types::layer::MembershipSource::Enumerated;
        match &input.artifacts {
            Some(ArtifactSource::File { path, fields }) => {
                read_artifacts(&input.name, path, fields, enumerated, &mut plan)?
            }
            Some(ArtifactSource::Inline(rows)) => plan_inline(&input.name, rows, &mut plan)?,
            // **Which artifacts exist is the layer's own artifact source's to say.** Without it a
            // member source would be both the roster and the population, and a mistyped key would
            // publish an artifact rather than fail to find one.
            None => {
                if let Some(members) = &input.members {
                    return Err(BuildError::Invalid(format!(
                        "{}: layer '{}' binds members with no artifacts of its own, which is what \
                         declares the artifacts they belong to; a key with no artifact behind it \
                         must be a refusal rather than a new artifact",
                        members.path.display(),
                        input.name
                    )));
                }
            }
        }
        if let Some(members) = &input.members {
            read_members(&input.name, &members.path, &members.fields, &mut plan)?;
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
) -> Result<()> {
    for batch in batches(path)? {
        let batch = batch?;
        let key = utf8(path, &batch, fields, "key")?;
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
            let address = address(path, layer, &level, key, row)?;
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
            plan.artifacts.insert(
                address.clone(),
                PlannedArtifact {
                    membership,
                    contents: match contents.as_ref() {
                        None => Vec::new(),
                        Some(column) => ranked_at(path, column, row, &address.2)?,
                    },
                    attached_to: attachment,
                    parent_key: parent.as_ref().and_then(|c| value_at(c, row)),
                },
            );
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
fn plan_inline(layer: &str, rows: &[InlineArtifact], plan: &mut LayerPlan) -> Result<()> {
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
        plan.artifacts.insert(
            address,
            PlannedArtifact {
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
            },
        );
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
fn read_members(layer: &str, path: &Path, fields: &Fields, plan: &mut LayerPlan) -> Result<()> {
    for batch in batches(path)? {
        let batch = batch?;
        let key = utf8(path, &batch, fields, "key")?;
        let level = optional_u32(path, &batch, LEVEL)?;
        let rank = optional_u32_field(path, &batch, fields, "rank")?;
        let entity = u64s(path, &batch, fields, "entity")?;

        for row in 0..batch.num_rows() {
            let address = address(path, layer, &level, key, row)?;
            // **The artifacts are the roster, and a key not on them is a refusal.** A
            // mistyped key would otherwise publish a phantom artifact carrying the members it
            // stole from a real one — an extra cluster nobody wrote, beside a real cluster
            // whose masked count is quietly short and which may fall below its own criterion
            // and vanish. Neither has an error anywhere to notice.
            let Some(entry) = plan.artifacts.get_mut(&address) else {
                return Err(BuildError::Invalid(format!(
                    "{}: names {} in level {} of {}, which the layer's artifacts do not declare",
                    path.display(),
                    address.2,
                    address.1,
                    address.0
                )));
            };
            // **A null `entity` is a refusal, not entity zero.** Arrow's `value` reads the values
            // buffer whatever the validity bitmap says, and a Parquet writer leaves a zero
            // there — so a producer whose join missed a row would publish the corpus's
            // lowest-numbered document into the cluster, moving its masked count for every
            // viewer who can see that one document.
            if entity.is_null(row) {
                return Err(BuildError::Invalid(format!(
                    "{}: {} has a null entity; a null is not entity zero, and publishing it as \
                     one puts a document nobody named into the artifact",
                    path.display(),
                    address.2
                )));
            }
            let source = entity.value(row);
            match rank.as_ref().and_then(|c| value_index(c, row)) {
                None => match &mut entry.membership {
                    PlannedMembership::Rows(members) => members.push(source),
                    // **Two answers to what an artifact's members are.** The config refuses the
                    // two declarations together; this is the same rule for a caller who bound the
                    // sources by hand, and it is fail-closed either way — taking one would make a
                    // masked count, and the criterion that divides by it, depend on which source
                    // the reader happened to read first.
                    _ => {
                        return Err(BuildError::Invalid(format!(
                            "{}: {} has a membership on its own row and another in this member \
                             source. They are two shapes of one thing, so which one a masked count \
                             divides by would be the order the sources were read",
                            path.display(),
                            address.2
                        )))
                    }
                },
                Some(index) => content_at_rank(entry, index).generated_from.push(source),
            }
        }
    }
    Ok(())
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
pub fn publish(
    plan: &LayerPlan,
    resolve: &dyn Fn(u64) -> Option<u64>,
    high_water: u64,
    prefix_dir: &Path,
    partition: &str,
    view: &str,
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

    // **Every membership is materialised here, before anything else looks at one.** A membership
    // declared by exclusion is complemented against the entity space this build assigned, once, so
    // the hierarchy checks below and the store beneath them see the same set an inclusion would
    // have written — and no later stage has a spelling left to learn.
    let mut resolved: BTreeMap<Address, ResolvedArtifact> = BTreeMap::new();
    for ((layer, level, key), artifact) in &plan.artifacts {
        resolved.insert(
            (layer.clone(), *level, key.clone()),
            resolve_artifact(layer, *level, key, artifact, resolve, high_water)?,
        );
    }

    // Grouped by `(layer, level)`, each level's artifacts in key order — so a level's
    // ordinals, and therefore its entities, are a function of the artifacts and never of the file's
    // row order.
    let mut batched: BTreeMap<(&str, u32), Vec<(&str, &ResolvedArtifact)>> = BTreeMap::new();
    for ((layer, level, key), artifact) in &resolved {
        batched
            .entry((layer.as_str(), *level))
            .or_default()
            .push((key.as_str(), artifact));
    }

    // **Published in declaration order, which is the order that honours `depends_on`.** An
    // attachment resolves against what is already published, so a label layer must follow the layer
    // it attaches into — and iterating the map instead would publish in alphabetical order, making
    // an operator's file work or fail on how their layers happen to sort.
    let mut order: Vec<(&str, u32)> = Vec::with_capacity(batched.len());
    for declaration in &plan.declarations {
        let name = declaration.name.as_str();
        order.extend(
            batched
                .keys()
                .filter(|(layer, _)| *layer == name)
                .copied(),
        );
    }

    for address in order {
        let (layer, level) = address;
        let artifacts = batched
            .remove(&address)
            .expect("every address came from the map a statement ago");
        let mut incoming = Vec::with_capacity(artifacts.len());
        for (key, artifact) in artifacts {
            incoming.push(incoming_artifact(key, artifact));
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

    let (violations, coverage) = verify_hierarchies(&plan.declarations, &resolved)?;
    let (layers, _tombstones) = registry.snapshot();
    let mut published = PublishedLayers {
        layers,
        containment_violations: violations,
        split_coverage: coverage,
        low_water: alloc.low_water(),
        ..PublishedLayers::default()
    };
    write_membership_extents(&store, prefix_dir, partition, &mut published)?;
    write_content_extent(&store, prefix_dir, partition, &mut published)?;
    Ok(published)
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
    for ((layer, _, key), artifact) in &plan.artifacts {
        let Some(depends_on) = declared.get(layer.as_str()) else {
            continue;
        };
        match &artifact.attached_to {
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
        let kind = kind_of.get(layer.as_str()).copied().unwrap_or(
            tessera_types::layer::HierarchyKind::Flat,
        );
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
        // **A set per parent, not a scan per member.** The membership test is the inner loop of
        // both checks below, and a linear `contains` over a parent holding the whole corpus makes
        // this pass quadratic in the level's largest artifact.
        let held: std::collections::HashSet<u64> =
            parent.members.iter().map(|e| e.raw()).collect();
        let mut covered: std::collections::HashSet<u64> =
            std::collections::HashSet::with_capacity(held.len());

        for child_address in children {
            let child = &artifacts[child_address];
            let (_, child_level, child_key) = child_address;
            let mut escaping = 0u64;
            for member in child.members.iter().map(|e| e.raw()) {
                if held.contains(&member) {
                    covered.insert(member);
                } else {
                    // Reported, not refused: the tree is real, its rollup guarantee is not.
                    escaping += 1;
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
            stray_members: (held.len() - covered.len()) as u64,
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
    for (layer, level, key) in artifacts.keys() {
        if !matches!(
            kind_of.get(layer.as_str()),
            Some(tessera_types::layer::HierarchyKind::Nested)
        ) {
            continue;
        }
        let bound = artifacts
            .keys()
            .filter(|(l, v, _)| l == layer && v == level)
            .count();
        let mut node = key.clone();
        for _ in 0..=bound {
            let Some(artifact) = artifacts.get(&(layer.clone(), *level, node.clone())) else {
                break;
            };
            match artifact.parent_key.as_deref() {
                None => break,
                Some(parent) => node = parent.to_string(),
            }
        }
        if artifacts
            .get(&(layer.clone(), *level, node.clone()))
            .and_then(|a| a.parent_key.as_deref())
            .is_some()
        {
            return Err(BuildError::Invalid(format!(
                "{layer} level {level}: the lineage above {key} does not reach a root within the \
                 level's own artifact count, so the edges hold a cycle — a tree has a root to \
                 descend a cut from and a cycle has none"
            )));
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
    resolve: &dyn Fn(u64) -> Option<u64>,
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
    fields: &Fields,
    canonical: &str,
) -> Result<&'a StringArray> {
    typed(
        path,
        required(path, batch, fields, canonical)?,
        fields.of(canonical),
    )
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
    let ids = values.as_any().downcast_ref::<UInt64Array>().ok_or_else(|| {
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
    let entries = entries.as_any().downcast_ref::<ListArray>().ok_or_else(|| {
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
    let strings = values.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
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

/// One row's `(layer, level, key)`.
///
/// **The layer is the source's own**, never a column: one file holds one layer, which is what
/// removes the discriminator and with it any way for a layer to ingest another's rows.
///
/// **A key is required**, and its absence is a refusal rather than a generated name: an artifact
/// published without one can be named by no edge and matched by no later build, and the caller is
/// the only party who knows what it should be called.
fn address(
    path: &Path,
    layer: &str,
    level: &Option<&UInt32Array>,
    key: &StringArray,
    row: usize,
) -> Result<Address> {
    if key.is_null(row) {
        return Err(BuildError::Invalid(format!(
            "{}: row {row} names no key; a build-published artifact carries the caller's own name \
             for it, which is what an edge into it names",
            path.display()
        )));
    }
    Ok((
        layer.to_string(),
        level.map_or(0, |c| number_at(c, row)),
        key.value(row).to_string(),
    ))
}
