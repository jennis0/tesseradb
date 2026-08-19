//! The annotation layer registry: what layers exist, what they declared, and who may know it.
//!
//! A layer is the unit of gate and lifecycle (`annotations.md` §2.1), and this is where those two
//! live. It holds no artifacts — a layer's population arrives later and by a different route — so
//! everything here is about **identity, reachability and removal**.
//!
//! ## A gate-failed name and a never-registered name are the same answer and the same work
//!
//! Reachability is resolved **once per session** into a set of names ([`ResolvedLayers`]), and a
//! request naming a layer does one lookup in that set. A layer the principal's terms do not satisfy
//! is not in the set; a layer nobody ever registered is not in the set either. The two therefore
//! cost one identical set probe and produce one identical answer, which is the property the design
//! asks for — *indistinguishable in outcome and in work* — obtained structurally rather than by
//! padding a branch.
//!
//! Getting this wrong is easy and quiet: resolving per request against the live registry would make
//! a gate-failed name cost a map lookup plus a term intersection, and a never-registered name cost
//! a map miss. Both return "no". Only one of them touches the layer's term list, and the difference
//! is a timing oracle over which names exist.
//!
//! **What the resolved set may not cache is the verdict.** A suppression against the layer's own
//! entity takes effect at the ack, so a live overlay check runs *ahead* of the cached resolution on
//! every request (`annotation-write-cycle.md` §6). The set says *this principal's terms reach this
//! layer*; it does not say *this layer is currently served*. Caching the second is the fail-open,
//! and it is why [`ResolvedLayers`] carries a version key rather than a decision.
//!
//! ## A dropped name never comes back
//!
//! Drop tombstones the name for ever. Bookmarks, edges and suppressions all travel by it, so a name
//! that once meant something must not come to mean something else — a recreated `clusters/topics`
//! would silently inherit every stale reference to the old one. The ids do not come back either:
//! the allocator is monotone with no free list, and reclaiming them is decision 0072's work, which
//! is settled and unbuilt.

use std::collections::{BTreeMap, BTreeSet};

use tessera_types::layer::{
    DeclarationError, EntityRun, LayerDeclaration, MembershipSource, RegisteredLayer, ReservedRuns,
};
use tessera_types::{EntityId, TermId};

use crate::alloc::{AllocError, Allocator};
use crate::membership::{serialise_members, ArtifactStore, IncomingArtifact};
use crate::wal::{PublishedArtifact, WalRecord};

/// Why a registry operation was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum RegistryError {
    /// The declaration is not internally coherent.
    Declaration(DeclarationError),
    /// A layer of this name is already registered.
    NameTaken(String),
    /// This name was dropped. It is refused for ever — see the module doc.
    NameTombstoned(String),
    /// No such live layer.
    NoSuchLayer(String),
    /// The layer holds no such level. Levels are dense from 0, so this is a caller naming one the
    /// declaration never reserved.
    NoSuchLevel { layer: String, level: u32 },
    /// Artifacts were offered to a layer whose membership is evaluated rather than enumerated.
    NotEnumerated { layer: String },
    /// This level already holds an artifact under that key, or the batch repeats it.
    DuplicateKey { layer: String, key: String },
    /// The batch's supplied content does not match what the layer declares.
    ///
    /// **Every case here is fail-closed and none is repairable by the service.** A layer publishes
    /// its content *kinds* in `/v1/meta` so a client knows what to draw, and that is only safe
    /// because no served artifact ever lacks one its layer declared — so an artifact arriving
    /// without a declared kind is refused rather than served short. A generating set where none is
    /// tested is refused for the opposite reason: a claim the service carries and never checks
    /// reads, to anyone auditing it, as a control that is running.
    Content { layer: String, detail: String },
    /// A layer named in `depends_on` is not registered. Refused at create rather than discovered at
    /// the first edge, because an edge's target must exist before the edge (`annotation-write-cycle.md`
    /// §5.0.4) and a dangling dependency is that ordering constraint already broken.
    MissingDependency { layer: String, depends_on: String },
    /// An artifact attaches to a layer its own layer did not declare in `depends_on`.
    ///
    /// **The declaration is what makes the dangling-replacement refusal sound** (§5.0.4): a
    /// replacement is refused where it would dangle a *declared* dependent, so an edge into a layer
    /// nobody declared is an edge nothing protects.
    UndeclaredAttachment { layer: String, target: String },
    /// An artifact declares no dependency in a layer that declares one.
    ///
    /// **Fail-closed, because a dependency edge is a visibility term**
    /// ([decision 0089](../../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)):
    /// a dependent is served only where the artifact it attaches to is served, so an artifact with
    /// no attachment has nothing for that prerequisite to gate on. Admitting it would make the
    /// prerequisite silently optional — the caller's producer error becoming a permission.
    MissingAttachment { layer: String, key: String },
    /// An artifact attaches to a target that does not exist — no such layer, no such level, or no
    /// artifact under that key.
    ///
    /// Refused rather than stored: an edge names a position in a dense level, so one written ahead
    /// of its target would name whatever later landed there.
    NoSuchAttachmentTarget {
        layer: String,
        target: String,
        level: u32,
        key: String,
    },
    /// A parent/child edge named a parent the layer does not hold where its declared shape says
    /// to look: the same level for a nested layer, a coarser one for a tiered layer.
    NoSuchParent {
        layer: String,
        level: u32,
        key: String,
    },
    /// An edge published into a layer that declares no lineage at all.
    EdgesOnUntreedLayer { layer: String, kind: String },
    /// A tiered layer whose parent key names an artifact in more than one coarser level.
    /// Refused rather than resolved by search order, which would make the edge's meaning depend on
    /// how the levels were walked.
    AmbiguousParent { layer: String, key: String },
    /// The entity space could not supply the layer's entity or its reserved runs.
    Alloc(AllocError),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Declaration(e) => write!(f, "{e}"),
            RegistryError::NameTaken(n) => write!(f, "a layer named {n} is already registered"),
            RegistryError::NameTombstoned(n) => write!(
                f,
                "the layer name {n} was dropped and cannot be reused: bookmarks, edges and \
                 suppressions travel by it, so reusing it would silently redirect them"
            ),
            RegistryError::NoSuchLayer(n) => write!(f, "no layer named {n}"),
            RegistryError::NoSuchLevel { layer, level } => {
                write!(f, "{layer} declares no level {level}")
            }
            RegistryError::NotEnumerated { layer } => write!(
                f,
                "{layer} derives its membership from a predicate, so it cannot be published into: \
                 an enumerated set beside a live predicate diverges from it at the first write"
            ),
            RegistryError::DuplicateKey { layer, key } => write!(
                f,
                "{layer} already holds an artifact keyed {key}; publication is append-only, and an \
                 edit is a delete plus a re-publish"
            ),
            RegistryError::Content { layer, detail } => {
                write!(f, "{layer}: {detail}")
            }
            RegistryError::MissingDependency { layer, depends_on } => write!(
                f,
                "{layer} declares depends_on {depends_on}, which is not registered — an edge's \
                 target must exist before the edge"
            ),
            RegistryError::UndeclaredAttachment { layer, target } => write!(
                f,
                "{layer} publishes an artifact attached into {target}, which it does not declare in \
                 depends_on — an attached artifact is withheld with its target, and a dependency \
                 nobody declared is one no replacement checks"
            ),
            RegistryError::MissingAttachment { layer, key } => write!(
                f,
                "{layer} declares depends_on, so every artifact it publishes attaches to one — and \
                 {key} attaches to nothing. A dependent is visible only where what it depends on \
                 is visible, so an artifact with no dependency would be gated on nothing"
            ),
            RegistryError::NoSuchAttachmentTarget {
                layer,
                target,
                level,
                key,
            } => write!(
                f,
                "{layer} publishes an artifact attached to {key} in level {level} of {target}, \
                 which holds no such artifact — a target exists before the edge into it, or the \
                 edge names whatever later lands there"
            ),
            RegistryError::NoSuchParent { layer, level, key } => write!(
                f,
                "{layer} publishes an artifact in level {level} whose parent is {key}, which the \
                 layer does not hold where its declared shape says to look — the same level for a \
                 nested layer, a coarser level for a tiered one"
            ),
            RegistryError::EdgesOnUntreedLayer { layer, kind } => write!(
                f,
                "{layer} is declared {kind} and so has no lineage, but an artifact names a \
                 parent: declare the layer nested if its edges run within a level, or \
                 tiered if they run between levels"
            ),
            RegistryError::AmbiguousParent { layer, key } => write!(
                f,
                "{layer} publishes an artifact whose parent {key} exists in more than one coarser \
                 level; which level the edge meant would depend on the search order, so it is \
                 refused rather than resolved"
            ),
            RegistryError::Alloc(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<DeclarationError> for RegistryError {
    fn from(e: DeclarationError) -> Self {
        RegistryError::Declaration(e)
    }
}

impl From<AllocError> for RegistryError {
    fn from(e: AllocError) -> Self {
        RegistryError::Alloc(e)
    }
}

/// The layer names one principal may know about, resolved once per session.
///
/// **A set of names and a version, and deliberately not a set of decisions.** See the module doc:
/// the live suppression check runs ahead of this on every request, so what is cached here is
/// reachability by terms, which only a registry edit can change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedLayers {
    names: BTreeSet<String>,
    /// The registry version this was resolved against. A session holding a resolution from an
    /// earlier version must re-resolve rather than serve from it — a gate edit that narrowed a
    /// layer would otherwise keep every open session on the pre-edit gate for its remaining life.
    version: u64,
}

impl ResolvedLayers {
    /// Whether this principal may know the layer exists. **One set probe, whatever the reason for
    /// a miss** — that identity is the point.
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether this resolution was computed against `version` and may still be used.
    pub fn is_current_for(&self, version: u64) -> bool {
        self.version == version
    }
}

/// What layers exist and what they declared.
#[derive(Debug, Clone, Default)]
pub struct LayerRegistry {
    layers: BTreeMap<String, RegisteredLayer>,
    tombstones: BTreeSet<String>,
    /// Where the next layer's own entity comes from: `[next, end)` of a block taken from the
    /// row-less region. Layer entities are handed out singly from here rather than from the
    /// allocator, because the allocator deals only in whole aligned blocks — see
    /// [`Allocator::allocate_rowless`] for why that division is where it is.
    entity_cursor: Option<EntityCursor>,
    version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntityCursor {
    next: u64,
    end: u64,
}

impl LayerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bumped by every registration and drop. A session's [`ResolvedLayers`] is valid only for the
    /// version it was resolved against.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredLayer> {
        self.layers.get(name)
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &RegisteredLayer)> {
        self.layers.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Whether this name has ever been dropped.
    pub fn is_tombstoned(&self, name: &str) -> bool {
        self.tombstones.contains(name)
    }

    pub fn tombstones(&self) -> impl Iterator<Item = &str> {
        self.tombstones.iter().map(String::as_str)
    }

    /// Validates a registration and allocates its ids, returning the WAL record that makes it
    /// durable. **It does not apply it** — the caller appends the record, fsyncs, and only then
    /// calls [`LayerRegistry::apply`]. Allocating before the append is what lets the record carry
    /// the ids explicitly, so replay applies what was decided rather than re-deriving it.
    ///
    /// On any error nothing has moved: the checks all run before the first allocation.
    pub fn prepare_create(
        &mut self,
        declaration: LayerDeclaration,
        alloc: &mut Allocator,
    ) -> Result<WalRecord, RegistryError> {
        declaration.validate()?;
        if self.layers.contains_key(&declaration.name) {
            return Err(RegistryError::NameTaken(declaration.name));
        }
        if self.tombstones.contains(&declaration.name) {
            return Err(RegistryError::NameTombstoned(declaration.name));
        }
        for dep in &declaration.depends_on {
            if !self.layers.contains_key(dep) {
                return Err(RegistryError::MissingDependency {
                    layer: declaration.name.clone(),
                    depends_on: dep.clone(),
                });
            }
        }

        // One reserved block per level. A level that fills it is extended by appending another
        // block later; the runs are a list from the start so that growth is an append rather than
        // a migration.
        let mut runs = Vec::with_capacity(declaration.run_count());
        for _ in 0..declaration.run_count() {
            let run = alloc.allocate_rowless(1)?;
            runs.push(ReservedRuns::from_runs(vec![EntityRun {
                start: run.start,
                end: run.end,
            }]));
        }
        let layer_entity = self.take_layer_entity(alloc)?;

        Ok(WalRecord::LayerCreate {
            declaration: Box::new(declaration),
            layer_entity,
            runs,
        })
    }

    /// Validates a batch of artifacts against the layer they are offered to, claims ordinals and
    /// entities for them, and returns the record that makes them durable — on
    /// [`prepare_create`]'s contract: the caller appends, syncs, and only then applies.
    ///
    /// **The whole batch or none of it.** Ordinals are claimed contiguously from the level's
    /// cursor, so a partial acceptance would leave the level's dense addressing describing
    /// artifacts that do not exist. Every check therefore runs before the first allocation, and a
    /// refusal spends nothing.
    ///
    /// **Publication is append-only here.** A key already in the level is refused rather than
    /// treated as an update: an edit is a delete plus a re-ingest (decision 0047), and the delete
    /// half is the write cycle's edit pass, which is a later stage's work. Silently replacing would
    /// be the fail-open reading — it would strand the old artifact's entity while callers still
    /// hold its `tessera_id`, so a suppression against the thing they were shown would land on
    /// nothing.
    ///
    /// [`prepare_create`]: LayerRegistry::prepare_create
    pub fn prepare_publish(
        &self,
        layer_name: &str,
        level: u32,
        incoming: &[IncomingArtifact],
        store: &ArtifactStore,
        alloc: &mut Allocator,
    ) -> Result<WalRecord, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;

        // A spatial or attribute layer's membership is *evaluated*, never enumerated — publishing
        // one would install a frozen answer beside a live predicate, and the two would diverge at
        // the first ingest. Refused at the boundary rather than reconciled later.
        if layer.declaration.membership != MembershipSource::Enumerated {
            return Err(RegistryError::NotEnumerated {
                layer: layer_name.to_string(),
            });
        }
        let runs = layer
            .runs
            .get(level as usize)
            .ok_or_else(|| RegistryError::NoSuchLevel {
                layer: layer_name.to_string(),
                level,
            })?;

        // Duplicate keys, against the level and against the rest of the batch. Both, because a
        // batch that repeats a key internally would otherwise publish two artifacts under one name
        // and leave the index pointing at whichever landed last.
        let mut within_batch = BTreeSet::new();
        for artifact in incoming {
            let Some(key) = &artifact.key else {
                continue;
            };
            if store.ordinal_of_key(layer_name, level, key).is_some() || !within_batch.insert(key) {
                return Err(RegistryError::DuplicateKey {
                    layer: layer_name.to_string(),
                    key: key.clone(),
                });
            }
        }

        // **What the layer declares is what every artifact must carry**, checked once here rather
        // than discovered per request. The three refusals are one rule read three ways: a client
        // draws what `/v1/meta` says the layer carries, so a served artifact must never lack a
        // declared kind, must never carry an undeclared one, and must never carry a generating set
        // nothing will test.
        let declared = &layer.declaration.content.supplied;
        let requires_all_members = declared
            .iter()
            .any(|s| s.require_member_visibility.requires_all_members());
        for (i, artifact) in incoming.iter().enumerate() {
            let refuse = |detail: String| {
                Err(RegistryError::Content {
                    layer: layer_name.to_string(),
                    detail: format!("artifact {i} of this batch: {detail}"),
                })
            };
            if declared.is_empty() && !artifact.contents.is_empty() {
                return refuse(
                    "carries supplied content, and this layer declares none — the kinds a client \
                     may draw come from the layer's declaration, so content under no declared kind \
                     could never be served"
                        .to_string(),
                );
            }
            if !declared.is_empty() && artifact.contents.is_empty() {
                return refuse(format!(
                    "carries no supplied content, and this layer declares {} kind(s); an artifact \
                     served without content its layer declares cannot be told apart from one whose \
                     content was withheld",
                    declared.len()
                ));
            }
            for (rank, content) in artifact.contents.iter().enumerate() {
                if content.values.len() != declared.len() {
                    return refuse(format!(
                        "contents[{rank}] supplies {} value(s) for {} declared kind(s); every \
                         entry is a whole description, and a viewer is served one of them \
                         entire or no artifact at all",
                        content.values.len(),
                        declared.len()
                    ));
                }
                if !requires_all_members && !content.generated_from.is_empty() {
                    return refuse(format!(
                        "contents[{rank}] declares a generating set, and none of this layer's \
                         content requires its members visible; a set that is never tested is a \
                         claim the service would carry without meaning"
                    ));
                }
                if requires_all_members && content.generated_from.is_empty() {
                    return refuse(format!(
                        "contents[{rank}] declares no generating set, and this layer's content \
                         requires every member visible; such content is served only to a viewer \
                         who can see everything it was generated from, and an empty set is \
                         satisfied by everyone"
                    ));
                }
            }
        }

        // **Attachments, resolved before anything is allocated.** The caller names a target by the
        // key they published it under — an ordinal is never disclosed (C8), so a key is the
        // only address they hold — and what is stored is the resolved `(level, ordinal, entity)`.
        // Resolving once here rather than per request is what makes the extra predicate term one
        // `verdict` lookup instead of a registry walk.
        let attachments: Vec<Option<crate::membership::Attachment>> = incoming
            .iter()
            .map(|artifact| {
                let Some(wanted) = &artifact.attached_to else {
                    // **A layer that declares a dependency publishes only dependents** (decision
                    // 0089). Refused here rather than served ungated: the serving predicate reads
                    // the prerequisite off the attachment, so an artifact carrying none would be
                    // the one artifact of a label layer that answered on its own conjuncts alone.
                    if !layer.declaration.depends_on.is_empty() {
                        return Err(RegistryError::MissingAttachment {
                            layer: layer_name.to_string(),
                            key: artifact.key.clone().unwrap_or_else(|| "<no key>".to_string()),
                        });
                    }
                    return Ok(None);
                };
                if !layer.declaration.depends_on.contains(&wanted.layer) {
                    return Err(RegistryError::UndeclaredAttachment {
                        layer: layer_name.to_string(),
                        target: wanted.layer.clone(),
                    });
                }
                let missing = || RegistryError::NoSuchAttachmentTarget {
                    layer: layer_name.to_string(),
                    target: wanted.layer.clone(),
                    level: wanted.level,
                    key: wanted.key.clone(),
                };
                let target = self.layers.get(&wanted.layer).ok_or_else(missing)?;
                let ordinal = store
                    .ordinal_of_key(&wanted.layer, wanted.level, &wanted.key)
                    .ok_or_else(missing)?;
                let entity = target
                    .runs
                    .get(wanted.level as usize)
                    .and_then(|runs| runs.entity_of(ordinal as u64))
                    .ok_or_else(missing)?;
                Ok(Some(crate::membership::Attachment {
                    layer: wanted.layer.clone(),
                    level: wanted.level,
                    ordinal,
                    entity: EntityId::new(entity),
                }))
            })
            .collect::<Result<_, _>>()?;

        let first_ordinal = store.next_ordinal(layer_name, level) as u64;
        let needed = first_ordinal + incoming.len() as u64;

        // **Parents, and which direction an edge may run is the layer's declaration.**
        //
        // A nested layer's edges relate artifacts of one level, and a level is normally published
        // in one batch — so a child's parent is usually a sibling in `incoming` that has no ordinal
        // until this call assigns one. Resolving the batch by position and falling through to the
        // store is what lets a level arrive whole or in pieces without the caller having to know
        // which.
        //
        // A tiered layer's edges run the other way: from a **coarser level** to this one.
        // Its parent was published in an earlier batch, so only the store can answer, and the
        // search runs over the levels above this one. A key found in two of them is a refusal
        // rather than a first-match, because which one an edge meant would then depend on the
        // search order.
        //
        // **A layer may not mix the two**, which is what makes the question answerable at all: the
        // declared kind says which shape its edges have, and an edge of the other shape refuses.
        let cross_level = matches!(
            layer.declaration.hierarchy.kind,
            tessera_types::layer::HierarchyKind::Tiered
        );
        let edges_allowed = cross_level
            || matches!(
                layer.declaration.hierarchy.kind,
                tessera_types::layer::HierarchyKind::Nested
            );
        let batch_ordinal = |key: &str| {
            incoming
                .iter()
                .position(|a| a.key.as_deref() == Some(key))
                .map(|i| first_ordinal as u32 + i as u32)
        };
        let parents: Vec<Option<crate::wal::ParentRef>> = incoming
            .iter()
            .map(|artifact| {
                let Some(key) = artifact.parent_key.as_deref() else {
                    return Ok(None);
                };
                let missing = || RegistryError::NoSuchParent {
                    layer: layer_name.to_string(),
                    level,
                    key: key.to_string(),
                };
                if !edges_allowed {
                    return Err(RegistryError::EdgesOnUntreedLayer {
                        layer: layer_name.to_string(),
                        kind: format!("{:?}", layer.declaration.hierarchy.kind).to_lowercase(),
                    });
                }
                // **Only a within-level edge can name itself.** A key is unique per
                // `(layer, level)`, so a levelled taxonomy legitimately carries the same key at two
                // levels — an arXiv archive with no subclass is `hep-ph` at both, and the level-1
                // artifact's parent is the level-0 one of the same name. Refusing that would force
                // a caller to rename half their taxonomy to satisfy a check meant for a tree.
                if !cross_level && artifact.key.as_deref() == Some(key) {
                    return Err(missing());
                }

                if cross_level {
                    let mut found = None;
                    for coarser in 0..level {
                        if let Some(ordinal) = store.ordinal_of_key(layer_name, coarser, key) {
                            if found.is_some() {
                                return Err(RegistryError::AmbiguousParent {
                                    layer: layer_name.to_string(),
                                    key: key.to_string(),
                                });
                            }
                            found = Some(crate::wal::ParentRef {
                                level: coarser,
                                ordinal,
                            });
                        }
                    }
                    // A key that exists only at this level or a finer one is an edge running the
                    // wrong way — refused rather than reinterpreted, since a tiered
                    // layer's whole guarantee is that lineage never runs against the levels.
                    return found.map(Some).ok_or_else(missing);
                }

                batch_ordinal(key)
                    .or_else(|| store.ordinal_of_key(layer_name, level, key))
                    .map(|ordinal| {
                        Some(crate::wal::ParentRef {
                            level,
                            ordinal,
                        })
                    })
                    .ok_or_else(missing)
            })
            .collect::<Result<_, _>>()?;

        // Extend the level's reservation if the batch outgrows it. The runs are a list from the
        // start precisely so this is an append rather than a migration — see `ReservedRuns`.
        let mut extend_runs = Vec::new();
        let mut capacity = runs.capacity();
        while capacity < needed {
            let block = alloc.allocate_rowless(1)?;
            capacity += block.end - block.start;
            extend_runs.push(EntityRun {
                start: block.start,
                end: block.end,
            });
        }

        // Resolve every ordinal against the *extended* runs, so an artifact landing in a block this
        // batch just claimed gets its entity from it rather than from a level that has not grown
        // yet.
        let mut extended = runs.clone();
        for run in &extend_runs {
            extended.push(*run);
        }

        let artifacts = incoming
            .iter()
            .enumerate()
            .map(|(i, artifact)| {
                let ordinal = first_ordinal + i as u64;
                let entity = extended
                    .entity_of(ordinal)
                    .expect("the reservation was extended to cover every ordinal in the batch");
                PublishedArtifact {
                    ordinal: ordinal as u32,
                    entity: EntityId::new(entity),
                    key: artifact.key.clone(),
                    members: serialise_members(&artifact.members),
                    contents: artifact
                        .contents
                        .iter()
                        .map(|v| crate::wal::PublishedContent {
                            values: v.values.clone(),
                            generated_from: serialise_members(&v.generated_from),
                        })
                        .collect(),
                    attached_to: attachments[i].as_ref().map(|a| {
                        crate::wal::PublishedAttachment {
                            layer: a.layer.clone(),
                            level: a.level,
                            ordinal: a.ordinal,
                            entity: a.entity,
                        }
                    }),
                    parent: parents[i],
                }
            })
            .collect();

        Ok(WalRecord::ArtifactPublish {
            layer: layer_name.to_string(),
            level,
            extend_runs,
            artifacts,
        })
    }

    /// Validates a drop and returns the record that makes it durable, on [`prepare_create`]'s
    /// contract: the caller appends and syncs before applying.
    ///
    /// [`prepare_create`]: LayerRegistry::prepare_create
    pub fn prepare_drop(&self, name: &str) -> Result<WalRecord, RegistryError> {
        if !self.layers.contains_key(name) {
            return Err(RegistryError::NoSuchLayer(name.to_string()));
        }
        Ok(WalRecord::LayerDrop {
            name: name.to_string(),
        })
    }

    /// Seeds from a manifest's complete current view, **before** WAL replay unions the records
    /// written since.
    ///
    /// The order is the rule, not a preference: every WAL record postdates any state a manifest
    /// carries, so seeding afterwards resurrects a layer that was dropped since the last
    /// publication — gate and all, reachable again by whoever the old declaration admitted. It is
    /// the same seed-before-replay ordering the overlay follows, and for the same reason.
    ///
    /// The version is set past every seeded layer's, so a subsequent registration cannot mint a
    /// version a session has already cached a resolution against.
    pub fn seed(&mut self, layers: &[RegisteredLayer], tombstones: &[String]) {
        for layer in layers {
            self.version = self.version.max(layer.version);
            self.layers
                .insert(layer.declaration.name.clone(), layer.clone());
        }
        self.tombstones.extend(tombstones.iter().cloned());
    }

    /// This registry as a manifest carries it: every live layer, and every name ever dropped.
    pub fn snapshot(&self) -> (Vec<RegisteredLayer>, Vec<String>) {
        (
            self.layers.values().cloned().collect(),
            self.tombstones.iter().cloned().collect(),
        )
    }

    /// Applies a durable registry record — the one path by which this state ever changes, taken by
    /// both the live write path and replay.
    ///
    /// **Replay applies, it does not re-derive.** The ids come from the record, so a registration
    /// lands on the same entities it was acked on, whatever the allocator's state; and a drop
    /// tombstones whether or not the create that preceded it is still in the log, because a
    /// tombstone that depended on seeing its own create would evaporate at the first rotation.
    pub fn apply(&mut self, record: &WalRecord) {
        match record {
            WalRecord::LayerCreate {
                declaration,
                layer_entity,
                runs,
            } => {
                self.layers.insert(
                    declaration.name.clone(),
                    RegisteredLayer {
                        declaration: (**declaration).clone(),
                        entity: *layer_entity,
                        runs: runs.clone(),
                        version: self.version + 1,
                    },
                );
                self.version += 1;
            }
            WalRecord::LayerDrop { name } => {
                self.layers.remove(name);
                self.tombstones.insert(name.clone());
                self.version += 1;
            }
            // A publication's only effect on the *registry* is the reservation it grew. The
            // artifacts themselves belong to the store, applied from the same record.
            //
            // **The version does not move.** It keys a session's cached reachability, and
            // publishing artifacts changes who may reach the layer not at all — bumping it would
            // invalidate every open session's resolution on every batch, which at a clustering's
            // publication rate is a re-resolve per request.
            WalRecord::ArtifactPublish {
                layer, level, extend_runs, ..
            } => {
                if extend_runs.is_empty() {
                    return;
                }
                let Some(registered) = self.layers.get_mut(layer) else {
                    return;
                };
                let Some(runs) = registered.runs.get_mut(*level as usize) else {
                    return;
                };
                for run in extend_runs {
                    runs.push(*run);
                }
            }
            _ => {}
        }
    }

    /// Resolves which layers this principal may know exist, once per session.
    ///
    /// `resolve_label` maps a gate label to its term, returning `None` for a label the dictionary
    /// does not hold — which makes the layer reachable by nobody. **Fail-closed, and the right
    /// answer**: a gate naming a term no document carries grants nothing, and treating an
    /// unresolvable label as "no gate" would publish every such layer to everyone.
    ///
    /// Satisfaction is **intersection** with the principal's satisfied set, never a conservative
    /// label join: a join yields an empty required set for a disjunctive gate and would admit every
    /// principal. That error has been made once already in this codebase, in the view gate, and
    /// was caught in review.
    pub fn resolve_for(
        &self,
        is_satisfied: impl Fn(TermId) -> bool,
        resolve_label: impl Fn(&str) -> Option<TermId>,
    ) -> ResolvedLayers {
        let names = self
            .layers
            .iter()
            .filter(|(_, layer)| match &layer.declaration.visibility {
                None => true,
                Some(label) => resolve_label(label).is_some_and(&is_satisfied),
            })
            .map(|(name, _)| name.clone())
            .collect();
        ResolvedLayers {
            names,
            version: self.version,
        }
    }

    /// Every live layer's own entity, for the caller that needs to ask the overlay whether any of
    /// them is suppressed.
    pub fn layer_entities(&self) -> impl Iterator<Item = (&str, EntityId)> {
        self.layers.iter().map(|(n, l)| (n.as_str(), l.entity))
    }

    /// Which artifact an entity addresses: `(layer, level, ordinal)`.
    ///
    /// **Addressing, never authorisation.** This answers where an entity sits and nothing about
    /// whether the caller may know it sits there — a drill-down resolves the address here and then
    /// puts it through the one predicate, in that order. Answering the address for an entity the
    /// caller may not see discloses nothing on its own: they supplied the identifier, and every
    /// route that acts on the answer gates first.
    ///
    /// A layer's *own* entity is deliberately not matched: it sits outside every level's run, which
    /// is what keeps suppressing a layer from suppressing artifact ordinal zero.
    ///
    /// O(layers × levels × runs) — a walk over a handful of ranges, with no per-artifact table in
    /// either direction, which is the property [`ReservedRuns`] is shaped to keep.
    pub fn locate(&self, entity: EntityId) -> Option<(&str, u32, u32)> {
        self.layers.iter().find_map(|(name, layer)| {
            layer.runs.iter().enumerate().find_map(|(level, runs)| {
                runs.ordinal_of(entity.raw())
                    .map(|ordinal| (name.as_str(), level as u32, ordinal as u32))
            })
        })
    }

    fn take_layer_entity(&mut self, alloc: &mut Allocator) -> Result<EntityId, AllocError> {
        let cursor = match self.entity_cursor {
            Some(c) if c.next < c.end => c,
            // No block, or the block is spent. Take another: one block holds 65 536 layers, so a
            // deployment reaches this a second time only if it has registered more layers than any
            // client could list.
            _ => {
                let run = alloc.allocate_rowless(1)?;
                EntityCursor {
                    next: run.start,
                    end: run.end,
                }
            }
        };
        self.entity_cursor = Some(EntityCursor {
            next: cursor.next + 1,
            end: cursor.end,
        });
        Ok(EntityId::new(cursor.next))
    }

    /// Rebuilds the layer-entity cursor after replay.
    ///
    /// **Replay cannot infer it from the records**, and this is the subtle part: a `LayerCreate`
    /// carries the entity it took, but not which block that entity came from nor how much of the
    /// block was left. Resuming from `max(entity) + 1` would be wrong the moment a drop retired the
    /// highest-numbered layer. So the cursor is reseeded conservatively — the next registration
    /// takes a fresh block — and the cost is at most one wasted block per restart, out of 65 536.
    /// The alternative, a durable cursor, buys back an id space nothing is short of.
    pub fn reseed_entity_cursor(&mut self) {
        self.entity_cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::layer::{
        ExistenceCriterion, Hierarchy, HierarchyKind, MembershipSource, RESERVED_BLOCK,
    };

    fn declaration(name: &str) -> LayerDeclaration {
        LayerDeclaration {
            name: name.into(),
            title: Some(name.into()),
            views: vec!["default".into()],
            membership: MembershipSource::Enumerated,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
            require_member_visibility: Some(ExistenceCriterion::Count(50)),
            hierarchy: Hierarchy {
                kind: HierarchyKind::Flat,
                prune_children: false,
            },
            content: Default::default(),
            depends_on: Vec::new(),
            levels: Vec::new(),
        }
    }

    fn gated(name: &str, label: &str) -> LayerDeclaration {
        let mut d = declaration(name);
        d.visibility = Some(label.into());
        d
    }

    /// Create + apply, the way the write path does it.
    fn register(
        reg: &mut LayerRegistry,
        alloc: &mut Allocator,
        d: LayerDeclaration,
    ) -> Result<(), RegistryError> {
        let record = reg.prepare_create(d, alloc)?;
        reg.apply(&record);
        Ok(())
    }

    #[test]
    fn a_layer_takes_one_entity_and_one_reserved_block_per_level() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();

        let layer = reg.get("clusters/a").unwrap();
        assert_eq!(layer.runs.len(), 1, "a level-less layer still holds level 0");
        assert_eq!(
            layer.runs[0].capacity(),
            tessera_types::layer::RESERVED_BLOCK
        );
        // The layer's own entity is not inside its level's run — it is a separate object, and
        // suppressing the layer must not suppress an artifact ordinal.
        assert!(layer.runs[0].ordinal_of(layer.entity.raw()).is_none());
        // Row-less through and through: everything allocated sits above the point region.
        assert!(layer.entity.raw() >= alloc.low_water());
        assert_eq!(alloc.high_water(), 0);
    }

    #[test]
    fn a_gate_failed_name_and_a_never_registered_name_are_one_set_probe_each() {
        // The property is that both are answered by the *same* operation on the same structure.
        // Anything that made the first consult the layer's terms at request time would be a timing
        // oracle over which names exist.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, gated("clusters/secret", "clearance:ts")).unwrap();
        register(&mut reg, &mut alloc, declaration("clusters/open")).unwrap();

        let resolved = reg.resolve_for(|t| t == TermId::new(7), |label| match label {
            "clearance:ts" => Some(TermId::new(99)),
            _ => None,
        });

        assert!(resolved.contains("clusters/open"));
        assert!(!resolved.contains("clusters/secret"));
        assert!(!resolved.contains("clusters/never-existed"));
        // The resolved set names only what may be known — a client cannot count what it cannot see.
        assert_eq!(resolved.names().collect::<Vec<_>>(), vec!["clusters/open"]);

        // And with the term: the same layer resolves.
        let cleared = reg.resolve_for(|t| t == TermId::new(7) || t == TermId::new(99), |label| match label {
            "clearance:ts" => Some(TermId::new(99)),
            _ => None,
        });
        assert!(cleared.contains("clusters/secret"));
    }

    #[test]
    fn a_gate_label_the_dictionary_does_not_hold_reaches_nobody() {
        // Fail-closed. Treating an unresolvable label as "no gate" would publish the layer to
        // everyone, which is the direction a mistake must never take.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, gated("clusters/x", "team:nobody")).unwrap();

        let resolved = reg.resolve_for(|_| true, |_| None);
        assert!(!resolved.contains("clusters/x"));
        assert_eq!(resolved.names().count(), 0);
    }

    #[test]
    fn a_resolution_does_not_outlive_the_registry_version_it_was_computed_from() {
        // A gate edit that narrows a layer must not leave open sessions on the pre-edit gate for
        // the rest of their life, which is what a resolution with no version key would do.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let resolved = reg.resolve_for(|_| false, |_| None);
        assert!(resolved.is_current_for(reg.version()));

        register(&mut reg, &mut alloc, declaration("clusters/b")).unwrap();
        assert!(!resolved.is_current_for(reg.version()));
    }

    #[test]
    fn a_dropped_name_is_refused_on_recreation_for_ever() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();

        let drop = reg.prepare_drop("clusters/a").unwrap();
        reg.apply(&drop);
        assert!(reg.get("clusters/a").is_none());
        assert!(reg.is_tombstoned("clusters/a"));

        assert_eq!(
            register(&mut reg, &mut alloc, declaration("clusters/a")),
            Err(RegistryError::NameTombstoned("clusters/a".into()))
        );
        // And dropping what is not there is a refusal, not a silent success.
        assert_eq!(
            reg.prepare_drop("clusters/a").unwrap_err(),
            RegistryError::NoSuchLayer("clusters/a".into())
        );
    }

    #[test]
    fn dropped_entity_ids_are_not_reclaimed() {
        // Decision 0072 is settled and unbuilt, so a drop must not lower the mark: the ids stay
        // spent, and a later registration gets fresh ones. Reclaiming without the
        // membership-reconciliation clause is the fail-open 0072 attaches as its condition.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let first = reg.get("clusters/a").unwrap().entity;
        let mark = alloc.low_water();

        let drop = reg.prepare_drop("clusters/a").unwrap();
        reg.apply(&drop);
        assert_eq!(alloc.low_water(), mark, "a drop frees nothing");

        register(&mut reg, &mut alloc, declaration("clusters/b")).unwrap();
        assert_ne!(reg.get("clusters/b").unwrap().entity, first);
    }

    #[test]
    fn replay_applies_the_recorded_ids_rather_than_reallocating() {
        // The durability contract: a registration lands on the entities it was acked on, whatever
        // the replaying allocator's state. Re-deriving would move a layer's ids under every
        // bookmark and suppression naming them.
        let mut live = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let create = live.prepare_create(declaration("clusters/a"), &mut alloc).unwrap();
        let drop_b = {
            let mut r = LayerRegistry::new();
            let mut a = Allocator::new(0);
            let c = r.prepare_create(declaration("clusters/b"), &mut a).unwrap();
            r.apply(&c);
            r.prepare_drop("clusters/b").unwrap()
        };
        live.apply(&create);

        // A fresh registry replaying the same log, with an allocator that has already moved on.
        let mut replayed = LayerRegistry::new();
        let mut other = Allocator::new(0);
        other.allocate_rowless(9).unwrap();
        for record in [&create, &drop_b] {
            replayed.apply(record);
        }

        assert_eq!(
            replayed.get("clusters/a").map(|l| l.entity),
            live.get("clusters/a").map(|l| l.entity)
        );
        assert_eq!(replayed.get("clusters/a"), live.get("clusters/a"));
        // A drop tombstones even though this log never carried its create — a tombstone that
        // needed to see its own create would evaporate at the first rotation.
        assert!(replayed.is_tombstoned("clusters/b"));
    }

    #[test]
    fn a_dependency_must_exist_before_the_layer_that_names_it() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut labels = declaration("topics/x");
        labels.depends_on = vec!["clusters/a".into()];

        assert_eq!(
            register(&mut reg, &mut alloc, labels.clone()),
            Err(RegistryError::MissingDependency {
                layer: "topics/x".into(),
                depends_on: "clusters/a".into(),
            })
        );
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        assert!(register(&mut reg, &mut alloc, labels).is_ok());
    }

    #[test]
    fn a_refused_registration_moves_nothing() {
        // Every check runs before the first allocation, so a rejected declaration leaves no ids
        // spent and no half-registered layer behind.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let mark = alloc.low_water();
        let version = reg.version();

        assert!(register(&mut reg, &mut alloc, declaration("clusters/a")).is_err());
        let mut bad = declaration("clusters/c");
        bad.hierarchy.kind = HierarchyKind::Stacked;
        assert!(register(&mut reg, &mut alloc, bad).is_err());

        assert_eq!(alloc.low_water(), mark);
        assert_eq!(reg.version(), version);
        assert_eq!(reg.len(), 1);
    }

    fn incoming(key: &str, members: &[u32]) -> IncomingArtifact {
        IncomingArtifact {
            key: Some(key.into()),
            members: croaring::Bitmap::of(members),
            contents: Vec::new(),
            attached_to: None,
            parent_key: None,
        }
    }

    /// Publish + apply into both structures, the way the executor does it.
    fn publish(
        reg: &mut LayerRegistry,
        store: &mut ArtifactStore,
        alloc: &mut Allocator,
        layer: &str,
        incoming: &[IncomingArtifact],
    ) -> Result<WalRecord, RegistryError> {
        let record = reg.prepare_publish(layer, 0, incoming, store, alloc)?;
        reg.apply(&record);
        assert_eq!(store.apply(&record, 0), 0);
        Ok(record)
    }

    /// **An attachment resolves to an address, and the target must already be there.** The caller
    /// names a key because that is all they hold; what is stored is `(level, ordinal, entity)`, so
    /// the extra visibility term is one lookup rather than a walk through the registry.
    #[test]
    fn an_attachment_resolves_to_its_targets_entity_and_the_target_must_exist_first() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let mut labels = declaration("topics/x");
        labels.depends_on = vec!["clusters/a".into()];
        register(&mut reg, &mut alloc, labels).unwrap();

        let label = |target: &str| {
            let mut artifact = incoming("l0", &[1, 2]);
            artifact.attached_to = Some(crate::membership::IncomingAttachment {
                layer: "clusters/a".into(),
                level: 0,
                key: target.into(),
            });
            artifact
        };

        // The cluster does not exist yet: refused, and the batch spends nothing.
        let mark = alloc.low_water();
        assert_eq!(
            publish(&mut reg, &mut store, &mut alloc, "topics/x", &[label("c0")]),
            Err(RegistryError::NoSuchAttachmentTarget {
                layer: "topics/x".into(),
                target: "clusters/a".into(),
                level: 0,
                key: "c0".into(),
            })
        );
        assert_eq!(alloc.low_water(), mark);

        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c0", &[1, 2, 3])],
        )
        .unwrap();
        publish(&mut reg, &mut store, &mut alloc, "topics/x", &[label("c0")]).unwrap();

        // The stored edge names the cluster's own entity — the identifier the predicate reads.
        let cluster = store.get("clusters/a", 0, 0).unwrap().entity;
        assert_eq!(
            store.get("topics/x", 0, 0).unwrap().attached_to,
            Some(crate::membership::Attachment {
                layer: "clusters/a".into(),
                level: 0,
                ordinal: 0,
                entity: cluster,
            })
        );
    }

    /// An edge into a layer nobody declared is an edge no replacement checks, so it is refused at
    /// publication rather than discovered when the target layer is replaced out from under it.
    #[test]
    fn an_attachment_into_an_undeclared_layer_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        // `topics/x` declares no dependency at all.
        register(&mut reg, &mut alloc, declaration("topics/x")).unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c0", &[1, 2, 3])],
        )
        .unwrap();

        let mut artifact = incoming("l0", &[1]);
        artifact.attached_to = Some(crate::membership::IncomingAttachment {
            layer: "clusters/a".into(),
            level: 0,
            key: "c0".into(),
        });
        assert_eq!(
            publish(&mut reg, &mut store, &mut alloc, "topics/x", &[artifact]),
            Err(RegistryError::UndeclaredAttachment {
                layer: "topics/x".into(),
                target: "clusters/a".into(),
            })
        );
    }

    #[test]
    fn ordinals_are_dense_across_batches_and_each_addresses_its_own_entity() {
        // Dense addressing is what makes an ordinal `entity − run.start` arithmetic rather than a
        // lookup table. Two batches must therefore continue one numbering, not restart it — a
        // second batch numbering from zero would overwrite the first's artifacts in place.
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();

        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c0", &[1, 2]), incoming("c1", &[3])],
        )
        .unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c2", &[4])],
        )
        .unwrap();

        assert_eq!(store.next_ordinal("clusters/a", 0), 3);
        assert_eq!(store.ordinal_of_key("clusters/a", 0, "c2"), Some(2));

        // Every ordinal resolves back to the entity the level's run gives it, and the layer's own
        // entity is none of them — suppressing the layer must not suppress an artifact.
        let layer = reg.get("clusters/a").unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for ordinal in 0..3u32 {
            let record = store.get("clusters/a", 0, ordinal).unwrap();
            assert_eq!(layer.runs[0].entity_of(ordinal as u64), Some(record.entity.raw()));
            assert!(seen.insert(record.entity));
            assert_ne!(record.entity, layer.entity);
        }
    }

    #[test]
    fn a_batch_that_outgrows_its_level_extends_the_reservation() {
        // A level holds 65 536 artifacts per block. A clustering larger than that must grow rather
        // than be refused, and the extension has to land in the record — an ordinal walks the runs
        // in *allocation* order, so a re-derived extension would renumber everything above it.
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let before = alloc.low_water();

        // Fill the first block, then step over its edge.
        let full: Vec<IncomingArtifact> = (0..RESERVED_BLOCK)
            .map(|i| incoming(&format!("c{i}"), &[i as u32]))
            .collect();
        let record = publish(&mut reg, &mut store, &mut alloc, "clusters/a", &full).unwrap();
        let WalRecord::ArtifactPublish { extend_runs, .. } = &record else {
            unreachable!()
        };
        assert!(extend_runs.is_empty(), "the first block was enough");
        assert_eq!(alloc.low_water(), before);

        let record = publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("over", &[9])],
        )
        .unwrap();
        let WalRecord::ArtifactPublish { extend_runs, .. } = &record else {
            unreachable!()
        };
        assert_eq!(extend_runs.len(), 1, "one more block");
        assert_eq!(alloc.low_water(), before - RESERVED_BLOCK);

        // The artifact past the edge lands in the new block, which sits *below* the first — the
        // allocator is monotone downward and hands nobody a reserved gap.
        let layer = reg.get("clusters/a").unwrap();
        assert_eq!(layer.runs[0].capacity(), 2 * RESERVED_BLOCK);
        let over = store.get("clusters/a", 0, RESERVED_BLOCK as u32).unwrap();
        assert_eq!(over.entity.raw(), extend_runs[0].start);
        assert!(over.entity.raw() < store.get("clusters/a", 0, 0).unwrap().entity.raw());
        assert_eq!(
            layer.runs[0].ordinal_of(over.entity.raw()),
            Some(RESERVED_BLOCK)
        );
    }

    #[test]
    fn a_key_already_in_the_level_or_repeated_in_the_batch_is_refused() {
        // Append-only: an edit is a delete plus a re-publish (decision 0047), and silently
        // replacing would strand the old artifact's entity while callers still hold its
        // `tessera_id` — so a suppression against what they were shown would land on nothing.
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c0", &[1])],
        )
        .unwrap();

        assert_eq!(
            publish(
                &mut reg,
                &mut store,
                &mut alloc,
                "clusters/a",
                &[incoming("c0", &[2])]
            ),
            Err(RegistryError::DuplicateKey {
                layer: "clusters/a".into(),
                key: "c0".into(),
            })
        );
        assert_eq!(
            publish(
                &mut reg,
                &mut store,
                &mut alloc,
                "clusters/a",
                &[incoming("c1", &[2]), incoming("c1", &[3])]
            ),
            Err(RegistryError::DuplicateKey {
                layer: "clusters/a".into(),
                key: "c1".into(),
            })
        );
        // Neither refusal moved anything: no ordinal claimed, no id spent.
        assert_eq!(store.next_ordinal("clusters/a", 0), 1);
    }

    #[test]
    fn a_predicate_layer_cannot_be_published_into() {
        // Its membership is evaluated, so an enumerated set beside it is a frozen answer that
        // diverges from the predicate at the first ingest.
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        let mut spatial = declaration("regions/uk");
        spatial.membership = MembershipSource::Spatial;
        spatial.require_member_visibility = Some(ExistenceCriterion::Count(25));
        register(&mut reg, &mut alloc, spatial).unwrap();

        assert_eq!(
            publish(
                &mut reg,
                &mut store,
                &mut alloc,
                "regions/uk",
                &[incoming("c0", &[1])]
            ),
            Err(RegistryError::NotEnumerated {
                layer: "regions/uk".into()
            })
        );
        // And a level the layer never declared is a refusal too, not an implicit creation.
        assert_eq!(
            reg.prepare_publish("clusters/nope", 0, &[], &store, &mut alloc),
            Err(RegistryError::NoSuchLayer("clusters/nope".into()))
        );
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        assert_eq!(
            reg.prepare_publish("clusters/a", 3, &[], &store, &mut alloc),
            Err(RegistryError::NoSuchLevel {
                layer: "clusters/a".into(),
                level: 3
            })
        );
    }

    #[test]
    fn replay_lands_a_publication_where_it_was_acked() {
        // The durability contract, on `replay_applies_the_recorded_ids_rather_than_reallocating`'s
        // argument: an artifact must come back on the entity its `tessera_id` was minted from,
        // whatever the replaying allocator's state.
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        let create = reg
            .prepare_create(declaration("clusters/a"), &mut alloc)
            .unwrap();
        reg.apply(&create);
        let publication = reg
            .prepare_publish(
                "clusters/a",
                0,
                &[incoming("c0", &[1, 2, 3]), incoming("c1", &[4])],
                &store,
                &mut alloc,
            )
            .unwrap();
        reg.apply(&publication);
        assert_eq!(store.apply(&publication, 900), 0);

        let mut replayed_reg = LayerRegistry::new();
        let mut replayed_store = ArtifactStore::new();
        let mut other = Allocator::new(0);
        other.allocate_rowless(9).unwrap();
        for record in [&create, &publication] {
            replayed_reg.apply(record);
            assert_eq!(replayed_store.apply(record, 900), 0);
        }

        assert_eq!(
            replayed_store.get("clusters/a", 0, 0).map(|r| r.entity),
            store.get("clusters/a", 0, 0).map(|r| r.entity)
        );
        assert_eq!(
            replayed_store.get("clusters/a", 0, 0).map(|r| &r.members),
            store.get("clusters/a", 0, 0).map(|r| &r.members)
        );
        assert_eq!(replayed_store.ordinal_of_key("clusters/a", 0, "c1"), Some(1));
        // And the pin comes back with it — the log may not be reclaimed past the publication.
        assert_eq!(replayed_store.oldest_wal_pos(), Some(900));
    }

    #[test]
    fn an_extension_lowers_the_replayed_mark_as_far_as_a_registration_does() {
        // The failure this closes: an extension block that raised no mark would be reissued on the
        // first restart after a large publication, with every artifact in it already suppressible
        // by a `tessera_id` a caller holds.
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        let create = reg
            .prepare_create(declaration("clusters/a"), &mut alloc)
            .unwrap();
        reg.apply(&create);
        let full: Vec<IncomingArtifact> = (0..RESERVED_BLOCK + 1)
            .map(|i| incoming(&format!("c{i}"), &[i as u32]))
            .collect();
        let publication = reg
            .prepare_publish("clusters/a", 0, &full, &store, &mut alloc)
            .unwrap();
        reg.apply(&publication);
        assert_eq!(store.apply(&publication, 0), 0);

        assert_eq!(
            crate::alloc::low_water_from(&[create.clone(), publication.clone()]),
            alloc.low_water()
        );
        // Reading the registration alone — the shape this function had before levels could grow —
        // leaves the extension block above the mark and so free to be reissued.
        assert!(crate::alloc::low_water_from(&[create]) > alloc.low_water());
    }

    #[test]
    fn layer_entities_come_from_one_block_until_it_is_spent() {
        // 65 536 layers per block, so the second block is a case a deployment reaches only in
        // theory — but the cursor has to roll over correctly or layer 65 537 collides with a level.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("a")).unwrap();
        let after_first = alloc.low_water();
        register(&mut reg, &mut alloc, declaration("b")).unwrap();

        // The second layer's *level* run took a block; its entity came from the block already held.
        assert_eq!(alloc.low_water(), after_first - tessera_types::layer::RESERVED_BLOCK);
        let a = reg.get("a").unwrap().entity.raw();
        let b = reg.get("b").unwrap().entity.raw();
        assert_eq!(b, a + 1);

        // After a restart the cursor is reseeded, so the next layer takes a fresh block rather than
        // guessing where the old one had got to.
        reg.reseed_entity_cursor();
        let before = alloc.low_water();
        register(&mut reg, &mut alloc, declaration("c")).unwrap();
        assert_eq!(
            alloc.low_water(),
            before - 2 * tessera_types::layer::RESERVED_BLOCK,
            "one block for the level, one fresh block for the entity cursor"
        );
        assert_ne!(reg.get("c").unwrap().entity.raw(), b + 1);
    }
}
