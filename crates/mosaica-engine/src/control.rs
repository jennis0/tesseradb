//! The control-plane calls: resolving, writing, declaring and publishing.

use mosaica_lifecycle::wal::ChangeOp;
use mosaica_store::StoreError;
use mosaica_types::{EntityId, ItemAt, LayerSpace, MosaicaId, ShardId, Target, TermId};

use crate::edited::{NamedTarget, Stamp};
use crate::engine::Engine;
use crate::resolver::{MemberNames, MemberSet, Names, Refused, Resolver, ScopeMasks, WriteScope};
use crate::write::joined::flushed_terms_of;
use crate::Generation;

impl Engine {
    /// Resolve raw term descriptors to `TermId`s: a dictionary hit returns the durable
    /// bundle-relative id; a miss is interned in this process's extension state and resumed
    /// across calls. `/control/ingest` resolves before the batch is durably appended. An
    /// extension id is never satisfiable by any session, so a live/replay mismatch only
    /// renumbers bookkeeping.
    pub fn resolve_terms(&self, descriptors: &[Vec<u8>]) -> Vec<TermId> {
        self.write
            .live()
            .resolve_terms(&self.generation.load().dict, descriptors)
    }

    /// A resolver of the names a writer of `scope` sends, against `generation`.
    pub(crate) fn resolver<'a>(
        &'a self,
        generation: &'a Generation,
        scope: &'a WriteScope,
    ) -> Resolver<'a> {
        Resolver::new(generation, scope, &self.identity_key, self.write.live())
    }

    /// `table`'s rows resolved for a write by a writer of `scope`, against the generation served
    /// now, and resolved again where the write applies wherever what they named has moved.
    /// `targets` lets a row name an artifact or a layer by its `mosaica_id`.
    pub fn names(
        &self,
        table: AddressTable,
        scope: &WriteScope,
        targets: bool,
    ) -> std::result::Result<Names, crate::EngineError> {
        // Only an existence criterion reads the fragments, so they are built only where a change
        // names an artifact.
        let names_an_artifact = targets
            && table
                .mosaica_id
                .iter()
                .flatten()
                .flatten()
                .any(|id| self.identity_key.invert_artifact(*id).is_some());
        let scope = match names_an_artifact {
            true => self.scope_masks(scope)?,
            false => scope.clone(),
        };
        let generation = self.generation.load_full();
        Names::resolve(&self.resolver(&generation, &scope), table, targets)
    }

    /// `scope` with each shard's fragment of its grant, as the generation served now holds them.
    pub(crate) fn scope_masks(
        &self,
        scope: &WriteScope,
    ) -> std::result::Result<WriteScope, crate::EngineError> {
        let WriteScope::Terms(terms, _) = scope else {
            return Ok(WriteScope::All);
        };
        #[cfg(feature = "fault-injection")]
        if self.switches.fragment_building_wanted() {
            return Err(crate::EngineError::FragmentBuilding);
        }
        let generation = self.generation.load_full();
        let masks = generation
            .shards()
            .map(|shard| Ok((shard.id(), shard.fragment(&self.fragments, terms.grant())?)))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(crate::session::fragment_error)?;
        Ok(WriteScope::Terms(
            std::sync::Arc::clone(terms),
            ScopeMasks::new(masks),
        ))
    }
}

/// The rows of a call that addresses items, as the caller wrote them: an optional `mosaica_id`
/// column and the other columns by name, `None` where a cell is null. An integer the field cannot
/// hold names nothing.
#[derive(Debug, Clone, Default)]
pub struct AddressTable {
    pub rows: usize,
    pub mosaica_id: Option<Vec<Option<MosaicaId>>>,
    /// `(column, cells)`, one cell per row.
    pub columns: Vec<(String, Vec<Option<AddressValue>>)>,
}

/// One cell of an address as the caller wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressValue {
    /// A keyword, or an integer or a timestamp written in decimal digits.
    Text(String),
    Integer(i128),
    /// A value no field is named by, such as a fraction or a boolean, as a refusal shows it. It
    /// names nothing, and is refused only in a unique field's column.
    Other(String),
}

impl Engine {
    /// Resolve `mosaica_id`s of items, artifacts and layers to what each names: `None` per
    /// position for an identifier that names nothing.
    pub fn resolve_targets(
        &self,
        ids: &[MosaicaId],
    ) -> std::result::Result<Vec<Option<Target>>, StoreError> {
        let generation = self.generation.load_full();
        let resolver = self.resolver(&generation, &WriteScope::All);
        let mut out: Vec<Option<Target>> = resolver
            .mosaica_items(ids)?
            .into_iter()
            .map(|item| item.map(ItemAt::target))
            .collect();
        let artifacts: Vec<(usize, (LayerSpace, EntityId))> = ids
            .iter()
            .enumerate()
            .filter(|(at, _)| out[*at].is_none())
            .filter_map(|(at, id)| Some((at, self.identity_key.invert_artifact(*id)?)))
            .collect();
        let named: Vec<(LayerSpace, EntityId)> = artifacts.iter().map(|(_, a)| *a).collect();
        for ((at, _), target) in artifacts.iter().zip(resolver.artifacts(&named)) {
            out[*at] = target;
        }
        Ok(out)
    }

    /// Does `view` hold a row for `item`, flushed or buffered?
    pub fn view_holds(&self, item: ItemAt, view: &str) -> bool {
        self.generation()
            .shard(item.shard)
            .is_some_and(|shard| crate::edited::holds_in_view(shard, item.entity, view))
    }

    /// An already-flushed entity's full term set in the one shard served, ascending, once the
    /// entity's own row has left the buffer. The full set, never a subset: comparing only the
    /// terms the writer named would accept a batch that dropped one.
    ///
    /// `None` where no layer holds a list — unknown, not empty. `Some(vec![])` means the item
    /// legitimately carries no label. A malformed layer is `None` and is logged, naming the
    /// artefact and not the entity.
    pub fn flushed_terms(&self, entity: EntityId) -> Option<Vec<TermId>> {
        flushed_terms_of(self.generation().only_shard(), entity)
    }

    /// The body hash `principal`'s batch id was accepted with, if it was: a batch replayed with
    /// the same bytes is reported as a replay. The executor decides the same question again,
    /// race-free.
    pub fn accepted_batch(&self, principal: &str, batch_id: &str) -> Option<[u8; 32]> {
        self.write
            .live()
            .accepted_batch(principal, batch_id)
            .map(|(hash, _)| hash)
    }

    /// The wire `mosaica_id` of the item `entity` holds in the one shard served: the permutation of
    /// its number, returned instead of the raw `EntityId`, which never crosses the trust boundary.
    pub fn mosaica_id_of(&self, entity: EntityId) -> std::result::Result<MosaicaId, StoreError> {
        let generation = self.generation.load_full();
        crate::edited::mosaica_ids_of(&self.identity_key, generation.only_shard(), &[entity])
            .map(|ids| ids[0])
    }

    /// Request a flush. Accepted at any time and executed promptly: the flag pulls the tick's
    /// deadline forward and the doorbell wakes an idle executor. The 202 means "accepted, not yet
    /// done"; the response never waits on the segment write. An operator trigger, rate-decoupled
    /// from ingest. [`Engine::request_flush_publication`] is the same request with its
    /// publication number; this drops the number.
    pub fn request_flush(&self) {
        let _ = self.request_flush_publication();
    }

    /// Request a compaction of every shard with something to reclaim, one after another, or of
    /// the shard the schedule ranks first where none has: the trigger `POST /control/compact`
    /// takes. A compaction gets its own thread rather than the shared pool, and the 202 means
    /// minutes to hours of IO still to come. At most one compaction is in flight. A request that
    /// arrives while one runs is held until it lands, and `fold_requested` stays true until the
    /// last shard the request names publishes.
    pub fn request_fold(&self) {
        self.request_compaction(None);
    }

    /// [`Engine::request_fold`], for `shard` alone where it names one.
    pub fn request_compaction(&self, shard: Option<ShardId>) {
        self.write.health().request_compaction(shard);
        self.write.wake();
    }

    /// [`Engine::accept_changes`] for one change to the item `entity` of a bundle of one shard.
    /// Over several shards an entity names no item alone, so this answers
    /// [`crate::write::AcceptError::Contract`] and changes nothing.
    pub fn accept_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        let only = {
            let generation = self.generation.load();
            let mut shards = generation.shards();
            match (shards.next(), shards.next()) {
                (Some(shard), None) => Some(shard.id()),
                _ => None,
            }
        };
        let Some(shard) = only else {
            return Err(crate::write::AcceptError::Contract(
                "an entity alone names no item in a bundle of several shards; name its shard with \
                 `accept_changes`"
                    .into(),
            ));
        };
        self.accept_changes(vec![(Target::Item(shard, entity), op)])
    }

    /// [`Engine::submit_changes`], waiting for its receipt. The request is one log record: durable
    /// whole or not at all. An `Err` does not mean nothing happened: where the append or fsync
    /// fails, every deletion and suppression is applied all the same. See `ExecError::Wal`.
    pub fn accept_changes(
        &self,
        changes: Vec<(Target, ChangeOp)>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.submit_changes(changes)?.wait().map(drop)
    }

    /// [`Engine::submit_changes_named`] for targets as the generation served now holds them, each
    /// item addressed by its `mosaica_id`.
    pub fn submit_changes(
        &self,
        changes: Vec<(Target, ChangeOp)>,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        let generation = self.generation.load_full();
        let (targets, ops): (Vec<Target>, Vec<ChangeOp>) = changes.into_iter().unzip();
        let named =
            NamedTarget::all(&self.identity_key, &generation, &targets).map_err(store_refusal)?;
        let names = Names::of_targets(Stamp::of(&generation), WriteScope::All, &named);
        self.submit_changes_named(names, ops, false)
    }

    /// Apply one `/control/changes` request, row `i` of `names` taking `ops[i]`, and wait for its
    /// receipt: the rows refused where the request applies, with their reasons. See
    /// [`Engine::submit_changes_named`].
    pub fn accept_changes_named(
        &self,
        names: Names,
        ops: Vec<ChangeOp>,
        strict: bool,
    ) -> std::result::Result<Refused, crate::write::AcceptError> {
        self.submit_changes_named(names, ops, strict)?.wait()
    }

    /// Enqueue one `/control/changes` request without waiting, so requests from several callers
    /// reach the deny lane together and share one fsync. Each row's names are brought to the
    /// generation the request applies in, so a change reaches the item it names where the item is
    /// then, however edits and folds have moved it, and none where the item is gone, which refuses
    /// the row. Where `strict`, a refused row refuses the request, and nothing is applied. Read
    /// `PendingChange::wait` before treating either half's `Err` as "nothing happened".
    ///
    /// A deletion of a layer's own entity is refused, and the request with it, before anything
    /// is applied: a layer is removed by dropping it.
    pub fn submit_changes_named(
        &self,
        names: Names,
        ops: Vec<ChangeOp>,
        strict: bool,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        let layer_deleted =
            ops.iter()
                .enumerate()
                .find_map(|(row, op)| match (names.verdict(row), op) {
                    (Ok(Target::Artifact(space, entity)), ChangeOp::Delete) => {
                        self.write.live().layer_of_own_entity(space, entity)
                    }
                    _ => None,
                });
        if let Some(name) = layer_deleted {
            return Err(crate::write::AcceptError::Exec(
                mosaica_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "this request deletes layer '{name}' by its mosaica_id; send DELETE \
                         /control/layers/{name} to remove a layer"
                    ),
                },
            ));
        }
        #[cfg(feature = "fault-injection")]
        self.switches.hold_write_check_if_wanted();
        self.write.submit_changes(Box::new(names), ops, strict)
    }

    /// One registered layer's declaration, by name, with no gate. Answers what a declaration
    /// says, never what is served: [`Engine::visible_layers`] resolves reachability.
    pub fn registered_layer(&self, name: &str) -> Option<mosaica_types::layer::RegisteredLayer> {
        self.write.live().registered_layer(name)
    }

    /// [`Engine::registered_layer`] where a writer of `scope` reaches the layer, and `None` where
    /// it does not, as for a layer nobody registered.
    pub fn reached_layer(
        &self,
        name: &str,
        scope: &WriteScope,
    ) -> Option<mosaica_types::layer::RegisteredLayer> {
        self.write.live().reached_layer(name, scope)
    }

    /// [`Engine::meta`] as a writer of `scope` holds it: the views and groups it reaches.
    pub fn meta_as(&self, scope: &WriteScope) -> crate::viewport::EngineMeta {
        let generation = self.generation.load_full();
        crate::viewport::meta_of(&generation)
            .within(scope.views(&generation.bundle.manifest).as_ref())
    }

    /// Register an annotation layer, returning its `mosaica_id`, the only address it can later
    /// be suppressed by — entity ids never cross the boundary.
    pub fn register_layer(
        &self,
        declaration: mosaica_types::layer::LayerDeclaration,
    ) -> std::result::Result<MosaicaId, crate::write::AcceptError> {
        let space = self.write.register_layer(declaration)?;
        self.issued_id(space, mosaica_types::layer::LAYER_ENTITY)
    }

    /// The `mosaica_id` of an entity of `space` a layer write just allocated, or the write's
    /// refusal.
    fn issued_id(
        &self,
        space: LayerSpace,
        entity: EntityId,
    ) -> std::result::Result<MosaicaId, crate::write::AcceptError> {
        self.artifact_id(space, entity).ok_or_else(|| {
            crate::write::AcceptError::Exec(mosaica_lifecycle::ExecError::LayerRefused {
                detail: "an allocated entity id lies outside the identity space".to_string(),
            })
        })
    }

    /// Drop a layer. Its name may be registered again, as a new layer whose identifiers share
    /// nothing with the dropped one's.
    pub fn drop_layer(&self, name: String) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.drop_layer(name)
    }

    /// Declare an attribute column while the service runs. `true` where a column of that name
    /// already carried exactly this identity and nothing was appended.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_attribute(
        &self,
        request: mosaica_lifecycle::AttributeRequest,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.write.declare_attribute(request)
    }

    /// Declare the point shard's size while the service runs. `true` where the shard already had
    /// this size and nothing was appended.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_shards(
        &self,
        size: u64,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.write.declare_shards(size)
    }

    /// Declare a vocabulary while the service runs.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_vocabulary(
        &self,
        request: mosaica_lifecycle::VocabularyRequest,
    ) -> std::result::Result<crate::write::VocabularyDeclared, crate::write::AcceptError> {
        self.write.declare_vocabulary(request)
    }

    /// A page of values for a vocabulary that exists, by a writer of `scope`.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<mosaica_lifecycle::DeclaredValue>,
        scope: &WriteScope,
    ) -> std::result::Result<crate::write::VocabularyValues, crate::write::AcceptError> {
        let mut page = self
            .write
            .mint_vocabulary_values(vocabulary.clone(), values)?;
        page.withheld = matches!(scope, WriteScope::Terms(..))
            && self
                .generation
                .load()
                .vocabularies
                .get(&vocabulary)
                .is_some_and(|held| {
                    held.visibility() == mosaica_types::vocabulary::Visibility::Derived
                });
        Ok(page)
    }

    /// Declare a view group while the service runs. The gate's labels are checked here, by the
    /// rule a build applies; every other rule is the write executor's.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_view_group(
        &self,
        mut declaration: mosaica_lifecycle::wal::ViewGroupDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        declaration.visibility = self.check_visibility(declaration.visibility.as_deref())?;
        declaration.point_default =
            self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_view_group(declaration)
    }

    /// Create a plain view while the service runs, on [`Engine::create_view_group`]'s rule.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_plain_view(
        &self,
        mut declaration: mosaica_lifecycle::wal::PlainViewDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        declaration.visibility = self.check_visibility(declaration.visibility.as_deref())?;
        declaration.point_default =
            self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_plain_view(declaration)
    }

    /// [`mosaica_access::point_default`]: the default as stored,
    /// or a view refusal.
    fn check_point_default(
        &self,
        default: Option<&str>,
    ) -> std::result::Result<Option<String>, crate::write::AcceptError> {
        default
            .map(mosaica_access::point_default)
            .transpose()
            .map_err(|detail| {
                crate::write::AcceptError::Exec(mosaica_lifecycle::ExecError::ViewRefused {
                    detail,
                })
            })
    }

    /// [`mosaica_access::declared_visibility`]: the gate as stored, or a
    /// view refusal.
    fn check_visibility(
        &self,
        visibility: Option<&[String]>,
    ) -> std::result::Result<Option<Vec<String>>, crate::write::AcceptError> {
        mosaica_access::declared_visibility(visibility).map_err(|detail| {
            crate::write::AcceptError::Exec(mosaica_lifecycle::ExecError::ViewRefused { detail })
        })
    }

    /// Create a view of a view group. Almost nothing is validated here; the gate's labels are the
    /// exception, checked at [`Engine::check_visibility`].
    pub fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, mosaica_types::view::ViewMetadataValue>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        let visibility = self.check_visibility(visibility.as_deref())?;
        self.write.create_view(group, key, visibility, metadata)
    }

    /// Drop a view, deleting the items it leaves in no view and its artifacts. Its key may be
    /// created again, as a new incarnation with none of the dropped view's rows or artifacts.
    pub fn drop_view(
        &self,
        group: String,
        key: String,
    ) -> std::result::Result<crate::write::ViewDropped, crate::write::AcceptError> {
        self.write.drop_view(group, key)
    }

    /// [`Engine::put_artifacts_named`] for members as the generation served now holds them, each
    /// addressed by its `mosaica_id`.
    pub fn put_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<mosaica_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        let generation = self.generation.load_full();
        let lists = artifacts.iter().enumerate().flat_map(|(at, artifact)| {
            let members = std::iter::once((at, MemberSet::Members, &artifact.members));
            let excluding = artifact
                .excluding
                .iter()
                .map(move |set| (at, MemberSet::Excluding, set));
            let sets = artifact
                .contents
                .iter()
                .enumerate()
                .map(move |(k, c)| (at, MemberSet::GeneratedFrom(k), &c.generated_from));
            members.chain(excluding).chain(sets)
        });
        let names =
            MemberNames::of_sets(&self.identity_key, &generation, lists).map_err(store_refusal)?;
        self.put_artifacts_named(layer, level, artifacts, names)
    }

    /// Put a batch of artifacts into one level of a layer, returning a `mosaica_id` per artifact,
    /// the batch's counts and the member rows refused. A held key is filled; an unheld key is
    /// published. Each artifact's sets are the items `names`' lists name where the batch applies:
    /// a member deleted since it was named names nothing, and is refused alone, or refuses the
    /// batch where its list is whole. A suppressed member is accepted.
    pub fn put_artifacts_named(
        &self,
        layer: String,
        level: u32,
        mut artifacts: Vec<mosaica_lifecycle::IncomingArtifact>,
        mut names: MemberNames,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        names.fill_artifacts(&mut artifacts, None);
        names.names.scope = self
            .scope_masks(names.names.scope())
            .map_err(crate::write::AcceptError::Engine)?;
        #[cfg(feature = "fault-injection")]
        self.switches.hold_write_check_if_wanted();
        let batch = self
            .write
            .publish_artifacts(layer, level, artifacts, Box::new(names))?;
        let mosaica_ids = batch
            .entities
            .into_iter()
            .map(|entity| self.issued_id(batch.space, entity))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(PublishedArtifacts {
            mosaica_ids,
            created: batch.created,
            without_content: batch.without_content,
            filled: batch.filled,
            joined: batch.joined,
            refused: batch.refused,
            unheld: batch.unheld,
        })
    }

    /// [`Self::put_artifacts`], answering the identifiers alone — the shape every caller that
    /// publishes new artifacts under new keys wants.
    pub fn publish_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<mosaica_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<Vec<MosaicaId>, crate::write::AcceptError> {
        self.put_artifacts(layer, level, artifacts)
            .map(|published| published.mosaica_ids)
    }

    /// [`Engine::grow_memberships_named`] for members as the generation served now holds them,
    /// each addressed by its `mosaica_id`, answering one [`GrownMembership`] per join applied.
    pub fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<mosaica_lifecycle::IncomingGrowth>,
    ) -> std::result::Result<Vec<GrownMembership>, crate::write::AcceptError> {
        let generation = self.generation.load_full();
        let lists = joins.iter().enumerate().flat_map(|(at, join)| {
            [
                (at, MemberSet::Members, &join.joining),
                (at, MemberSet::Leaving, &join.leaving),
            ]
        });
        let names =
            MemberNames::of_sets(&self.identity_key, &generation, lists).map_err(store_refusal)?;
        self.grow_memberships_named(layer, level, joins, names)
            .map(|grown| grown.grown)
    }

    /// Add points to the memberships of artifacts that already exist, each named by its published
    /// key. A suppressed artifact grows like any other and stays suppressed: the key resolves
    /// against the store, not what is served. The members are `names`' lists, on
    /// [`Engine::put_artifacts_named`]'s rule. A point may also name its artifacts via
    /// `/control/ingest`'s column; this entry point is for correcting points that already exist,
    /// and an unknown key is refused here regardless.
    ///
    /// One [`GrownMembership`] per join not refused for its label, the member rows refused, and
    /// the positions of the joins so refused. Neither an ordinal nor a membership size crosses the
    /// boundary.
    pub fn grow_memberships_named(
        &self,
        layer: String,
        level: u32,
        mut joins: Vec<mosaica_lifecycle::IncomingGrowth>,
        mut names: MemberNames,
    ) -> std::result::Result<GrownMemberships, crate::write::AcceptError> {
        names.fill_growth(&mut joins, None);
        names.names.scope = self
            .scope_masks(names.names.scope())
            .map_err(crate::write::AcceptError::Engine)?;
        #[cfg(feature = "fault-injection")]
        self.switches.hold_write_check_if_wanted();
        let batch = self
            .write
            .grow_memberships(layer, level, joins, Box::new(names))?;
        let grown = batch
            .grown
            .into_iter()
            .map(|receipt| {
                Ok(GrownMembership {
                    mosaica_id: self.issued_id(batch.space, receipt.entity)?,
                    joined: receipt.joined,
                    filled: receipt.filled,
                    left: receipt.left,
                    withdrawn: receipt.withdrawn,
                })
            })
            .collect::<std::result::Result<Vec<_>, crate::write::AcceptError>>()?;
        Ok(GrownMemberships {
            grown,
            refused: batch.refused,
            unheld: batch.unheld,
        })
    }

    /// Where an artifact target sits, and what was published there; `None` for an item. Addressing
    /// only: this says an entity is artifact *n* of a level; it says nothing about
    /// whether the asker may know that, and every route acting on the answer puts it through the
    /// visibility predicate first. The membership itself is not returned: a caller with a raw
    /// member set could count it, and an unmasked count over items a principal may not see leaks.
    pub fn locate_artifact(&self, target: Target) -> Option<PublishedArtifactAddress> {
        let Target::Artifact(space, entity) = target else {
            return None;
        };
        let (level, ordinal) = self.write.live().locate_artifact(space, entity)?;
        let layer = self.write.live().layer_in_space(space)?;
        let key = self.write.live().with_artifacts(|store| {
            store
                .get(&layer, level, ordinal)
                .and_then(|r| r.key.clone())
        });
        Some(PublishedArtifactAddress {
            layer,
            level,
            ordinal,
            key,
        })
    }
}

/// One join's answer from [`Engine::grow_memberships`]. `joined` is bounded by the members the
/// caller sent, so it says nothing about the members they did not: a membership size never
/// crosses the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrownMembership {
    /// The artifact's identifier, the same one its publication answered with.
    pub mosaica_id: MosaicaId,
    /// How many of the joining members were not already in the membership.
    pub joined: u64,
    /// How many of the fixed parts the join carried were absent and are now held.
    pub filled: u64,
    /// How many of the leaving members a generating-set page took out of it.
    pub left: u64,
    /// The rank this page emptied, where it emptied one: the content is withdrawn, its rank is
    /// never taken again, and the caller supplies it again at a new one.
    pub withdrawn: Option<u16>,
}

/// What [`Engine::grow_memberships_named`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrownMemberships {
    /// One per join not in `unheld`, in the caller's order.
    pub grown: Vec<GrownMembership>,
    /// The member rows that named nothing where the growth applied.
    pub refused: Refused,
    /// The joins, by position, refused for an access label the writer's terms do not satisfy,
    /// which wrote nothing.
    pub unheld: Vec<usize>,
}

/// What [`Engine::put_artifacts`] answers: the identifiers in the caller's order and the batch's
/// counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifacts {
    /// One per artifact not in `unheld`, in the caller's order: a held artifact's own, or the new
    /// one's.
    pub mosaica_ids: Vec<MosaicaId>,
    /// How many artifacts the batch created; the rest were held.
    pub created: u64,
    /// How many created artifacts carry no content on a layer declaring some.
    pub without_content: u64,
    /// How many fixed parts were filled on held artifacts.
    pub filled: u64,
    /// How many memberships the batch added: every member of a created artifact, and every member
    /// a held artifact did not already hold.
    pub joined: u64,
    /// The member rows that named nothing where the batch applied.
    pub refused: Refused,
    /// The artifacts, by position in the batch, refused for an access label the writer's terms
    /// do not satisfy, which wrote nothing.
    pub unheld: Vec<usize>,
}

/// Where an artifact sits, as [`Engine::locate_artifact`] answers it. The ordinal is here because
/// the engine's own routes address by it. It does not cross the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifactAddress {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    pub key: Option<String>,
}

/// A store failure while a write's names were read, as the write's refusal.
fn store_refusal(e: StoreError) -> crate::write::AcceptError {
    crate::write::AcceptError::Exec(mosaica_lifecycle::ExecError::Store {
        detail: e.to_string(),
    })
}

/// A layer declaration's views as `meta` holds them, as a build takes them: a view group named
/// stands for every view the group holds now ([`mosaica_types::layer::expand_views`]), a layer
/// scoped to a group names only views holding its keys, and a shape layer's views share one
/// space. The refusal says what to name instead.
pub(crate) fn declared_views(
    meta: &crate::viewport::EngineMeta,
    declaration: &mut mosaica_types::layer::LayerDeclaration,
) -> std::result::Result<(), String> {
    let name = declaration.name.clone();
    let declared_views = std::mem::take(&mut declaration.views);
    declaration.views = mosaica_types::layer::expand_views(
        &declared_views,
        |view| {
            meta.groups
                .iter()
                .find(|g| g.name == view)
                .map(|g| g.views.clone())
        },
        |view| meta.resolve_view(view).is_some(),
    )
    .map_err(|view| {
        format!(
            "layer '{name}' declares view '{view}', which is neither a view nor a view group of \
             this deployment; name one of: {}",
            meta.views
                .iter()
                .map(|v| v.id.as_str())
                .chain(meta.groups.iter().map(|g| g.name.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if let Some(group) = declaration.scope.group() {
        mosaica_types::layer::check_scoped_views(
            &declared_views,
            group,
            meta.groups
                .iter()
                .map(|g| (g.name.as_str(), g.members_of.as_deref())),
            |view| {
                meta.resolve_view(view)
                    .and_then(|v| v.roster.as_ref())
                    .map(|roster| roster.group.clone())
            },
        )
        .map_err(|outside| {
            format!(
                "layer '{name}' is scoped to group '{group}' and names view '{}', which holds no \
                 key of it; name {} or a view of {}, or drop the scope",
                outside.view,
                outside.sharing.join(" or "),
                if outside.sharing.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            )
        })?;
    }
    // A shape layer over both projected and unprojected views is refused at the declaration
    // rather than at its first artifact.
    if declaration.membership == mosaica_types::layer::MembershipSource::Spatial {
        if declaration.views.is_empty() {
            return Err(format!(
                "layer '{name}' declares no view to publish a shape into; name one"
            ));
        }
        let frames: Vec<crate::shapes::ViewFrame> = declaration
            .views
            .iter()
            .filter_map(|view| meta.resolve_view(view))
            .map(|view| {
                let q = view.quantisation;
                crate::shapes::ViewFrame::new(
                    &view.id,
                    view.projection,
                    crate::shapes::Bounds {
                        x_min: q.x_min,
                        x_max: q.x_max,
                        y_min: q.y_min,
                        y_max: q.y_max,
                    },
                )
            })
            .collect();
        crate::shapes::check_shape_span(&frames, crate::shapes::ShapeSpace::Wgs84)
            .map_err(|e| format!("layer '{name}': {e}"))?;
    }
    Ok(())
}
