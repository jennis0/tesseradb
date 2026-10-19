//! Every write's names, resolved where the write applies. A handler resolves a write's names
//! once into [`Names`] for one writer's [`WriteScope`]; the write thread, the only one that moves
//! an item, resolves again ([`Names::now`]) the rows whose item has since been deleted, edited or
//! folded away, or whose value has gained a holder.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use croaring::Bitmap;
use mosaica_authz::{FrozenFragment, Grant};
use mosaica_lifecycle::membership::{IncomingArtifact, IncomingGrowth};
use mosaica_lifecycle::resolve::{self, Batch, Holder, Reason, Refusal, RowIdentity, Verdict};
use mosaica_lifecycle::ShardSets;
use mosaica_store::manifest::DeclaredScalar;
use mosaica_store::unique::UniqueKey;
use mosaica_store::StoreError;
use mosaica_types::{
    EntityId, IdentityKey, ItemAt, LayerSpace, MosaicaId, ShardId, Target, TermId,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::control::{AddressTable, AddressValue};
use crate::edited::{NamedTarget, Stamp};
use crate::gate::VisibleViews;
use crate::write::LiveState;
use crate::{EngineError, Generation};

/// Whose terms bound what a write names. [`WriteScope::All`] names every item, as a build does:
/// the scope of a writer holding `write-all`. [`WriteScope::Terms`] names the items a session
/// authorised for the writer's terms may be served, suppressed ones included, so a writer may lift
/// a suppression it could not see.
#[derive(Debug, Clone, Default)]
pub enum WriteScope {
    #[default]
    All,
    /// The writer's terms, and each shard's fragment of their grant where the write tests an
    /// artifact's members ([`crate::Engine::scope_masks`]).
    Terms(Arc<TermsScope>, ScopeMasks),
}

impl WriteScope {
    /// Whether `other` is this scope: both [`WriteScope::All`], or the one scope built for one set
    /// of terms.
    pub(crate) fn is(&self, other: &WriteScope) -> bool {
        match (self, other) {
            (WriteScope::All, WriteScope::All) => true,
            (WriteScope::Terms(a, _), WriteScope::Terms(b, _)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Whether the writer holds `term`: every term, for [`WriteScope::All`].
    fn holds(&self, term: &str) -> bool {
        match self {
            WriteScope::All => true,
            WriteScope::Terms(scope, _) => scope.holds(term),
        }
    }

    /// Whether the writer satisfies one of the stored access labels `labels`; always for
    /// [`WriteScope::All`], even where a label does not parse.
    pub(crate) fn admits<S: AsRef<str>>(&self, labels: &[S]) -> bool {
        match self {
            WriteScope::All => true,
            WriteScope::Terms(..) => mosaica_access::admits(labels, &|term| self.holds(term)),
        }
    }

    /// Whether the writer may know a layer of `declaration` exists, as a session's layer gate
    /// reads its `visibility`.
    pub(crate) fn reaches_layer(
        &self,
        declaration: &mosaica_types::layer::LayerDeclaration,
    ) -> bool {
        declaration.reached_by(|label| self.admits(&[label]))
    }

    /// The views and groups of `manifest` the writer reaches, as a session holding its terms is
    /// gated ([`crate::gate::resolve`]); `None` for [`WriteScope::All`], which reaches all.
    pub(crate) fn views(
        &self,
        manifest: &mosaica_store::manifest::Manifest,
    ) -> Option<VisibleViews> {
        match self {
            WriteScope::All => None,
            WriteScope::Terms(..) => Some(crate::gate::resolve(manifest, &|term| self.holds(term))),
        }
    }

    /// Whether an artifact of a layer of `declaration` carrying the stored labels `access` admits
    /// the writer, on a session's label gate: one with none by the layer's
    /// `artifact_visibility.default`.
    pub(crate) fn admits_artifact_label(
        &self,
        declaration: &mosaica_types::layer::LayerDeclaration,
        access: &[Vec<u8>],
    ) -> bool {
        if matches!(self, WriteScope::All) {
            return true;
        }
        if access.is_empty() {
            return crate::artifacts::unlabelled_admitted(declaration, |label| {
                self.admits(&[label])
            });
        }
        let labels: Vec<std::borrow::Cow<'_, str>> = access
            .iter()
            .map(|label| String::from_utf8_lossy(label))
            .collect();
        self.admits(&labels)
    }
}

/// What a set of terms satisfies, as a writer's scope holds it: the index keys of the dictionary it
/// was built against, and the terms, by which a key promoted since is judged.
pub struct TermsScope {
    /// The keys satisfied, ascending: the grant whose fragment masks a set of items at once.
    grant: Grant,
    /// Every term held, and `public`.
    credentials: FxHashSet<Vec<u8>>,
    /// The dictionary's length when the scope was built.
    dict_len: u32,
}

impl std::fmt::Debug for TermsScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermsScope")
            .field("keys", &self.grant.keys().len())
            .field("dict_len", &self.dict_len)
            .finish_non_exhaustive()
    }
}

impl TermsScope {
    pub(crate) fn new(
        keys: &FxHashSet<TermId>,
        credentials: FxHashSet<Vec<u8>>,
        dict_len: u32,
    ) -> TermsScope {
        TermsScope {
            grant: crate::session::grant_of(keys),
            credentials,
            dict_len,
        }
    }

    pub(crate) fn grant(&self) -> &Grant {
        &self.grant
    }

    fn holds(&self, term: &str) -> bool {
        self.credentials.contains(term.as_bytes())
    }

    /// Whether the terms satisfy the index key `descriptor`: a term, `public`, or a conjunction's
    /// key.
    fn satisfies_key(&self, descriptor: &[u8]) -> bool {
        match mosaica_authz::label::label_of_key(descriptor) {
            Some(text) => mosaica_access::admits(&[text], &|term| self.holds(term)),
            None => self.credentials.contains(descriptor),
        }
    }

    /// Whether the terms satisfy an item indexed under `keys` in `generation`. A key promoted since
    /// the scope was built is judged by what it indexes.
    fn admits_keys(&self, keys: &[TermId], generation: &Generation, live: &LiveState) -> bool {
        let mut novel: FxHashSet<TermId> = FxHashSet::default();
        let dict = &generation.dict;
        for &key in keys {
            if self.grant.keys().binary_search(&key).is_ok() {
                return true;
            }
            if key.raw() < self.dict_len {
                continue;
            }
            if key.raw() < dict.len() {
                if crate::session::key_satisfied(dict, key, &self.credentials) {
                    return true;
                }
            } else {
                novel.insert(key);
            }
        }
        !novel.is_empty()
            && live
                .descriptors_of(&novel)
                .values()
                .any(|descriptor| self.satisfies_key(descriptor))
    }

    /// Whether a key promoted since the scope was built in `generation`'s dictionary is one the
    /// terms satisfy: the scope then names fewer items than the terms do, and is built again.
    fn is_stale(&self, generation: &Generation) -> bool {
        let dict = &generation.dict;
        (self.dict_len..dict.len())
            .map(TermId::new)
            .any(|key| crate::session::key_satisfied(dict, key, &self.credentials))
    }
}

/// Each shard's fragment of a scoped writer's grant, built where the write is admitted, so the write
/// thread masks a set by it and builds none.
#[derive(Clone, Default)]
pub struct ScopeMasks(Vec<(ShardId, Arc<FrozenFragment>)>);

impl std::fmt::Debug for ScopeMasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(shard, _)| shard))
            .finish()
    }
}

impl ScopeMasks {
    pub(crate) fn new(fragments: Vec<(ShardId, Arc<FrozenFragment>)>) -> ScopeMasks {
        ScopeMasks(fragments)
    }
}

/// The scopes built, by the terms they were built for.
#[derive(Default)]
pub(crate) struct WriteScopes(std::sync::Mutex<FxHashMap<Vec<String>, Arc<TermsScope>>>);

/// How many scopes are kept. Past it every kept scope is dropped, and each is built again when
/// next asked for.
const WRITE_SCOPES_KEPT: usize = 256;

impl WriteScopes {
    /// The scope of `terms` in `generation`: the one kept, unless a key promoted since is one the
    /// terms satisfy, and otherwise `build`'s.
    pub(crate) fn get_or_build(
        &self,
        terms: &[String],
        generation: &Generation,
        build: impl FnOnce() -> TermsScope,
    ) -> WriteScope {
        let kept = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(terms)
            .cloned();
        if let Some(kept) = kept.filter(|kept| !kept.is_stale(generation)) {
            return WriteScope::Terms(kept, ScopeMasks::default());
        }
        let built = Arc::new(build());
        let mut kept = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if kept.len() >= WRITE_SCOPES_KEPT {
            kept.clear();
        }
        kept.insert(terms.to_vec(), Arc::clone(&built));
        WriteScope::Terms(built, ScopeMasks::default())
    }
}

/// Who holds what in one generation, for a writer of one scope.
pub(crate) struct Resolver<'a> {
    pub(crate) generation: &'a Generation,
    pub(crate) scope: &'a WriteScope,
    pub(crate) key: &'a IdentityKey,
    pub(crate) live: &'a LiveState,
}

/// What a writer may name among a registry's layers and its store's artifacts
/// ([`mosaica_lifecycle::Reach`]): an artifact a session holding the writer's terms would be served
/// ([`crate::artifacts::verdict`]), suppression aside.
pub(crate) struct ArtifactReach<'r> {
    resolver: &'r Resolver<'r>,
    /// The spaces and declarations of the layers the writer reaches, by name; empty for
    /// [`WriteScope::All`], which reaches every one.
    layers: FxHashMap<String, (LayerSpace, mosaica_types::layer::LayerDeclaration)>,
    store: &'r mosaica_lifecycle::ArtifactStore,
    views: Option<VisibleViews>,
}

impl<'r> ArtifactReach<'r> {
    /// The reach of `resolver`'s writer over `registry`'s layers, as they stand now, and `store`.
    pub(crate) fn new(
        resolver: &'r Resolver<'r>,
        registry: &mosaica_lifecycle::LayerRegistry,
        store: &'r mosaica_lifecycle::ArtifactStore,
    ) -> ArtifactReach<'r> {
        let layers = match resolver.scope {
            WriteScope::All => FxHashMap::default(),
            WriteScope::Terms(..) => registry
                .iter()
                .filter(|(_, held)| resolver.scope.reaches_layer(&held.declaration))
                .map(|(name, held)| (name.to_string(), (held.space, held.declaration.clone())))
                .collect(),
        };
        ArtifactReach {
            resolver,
            layers,
            store,
            views: resolver.scope.views(&resolver.generation.bundle.manifest),
        }
    }

    /// How many of `set`, entities of `shard`, are inside the scope, deleted ones aside.
    fn within(&self, shard: ShardId, set: &Bitmap) -> u64 {
        let mut set = set.clone();
        if let Some(held) = self.resolver.generation.shard(shard) {
            set.andnot_inplace(held.overlay.deleted_set());
        }
        self.resolver.mask(shard, &mut set);
        set.cardinality()
    }

    /// Whether a session holding the writer's terms would be served the artifact at `ordinal` of
    /// `level` of `layer` in a view of `within`, every view where it is `None`, following at most
    /// `depth` attachments.
    fn served(
        &self,
        layer: &str,
        level: u32,
        ordinal: u32,
        within: Option<&[&str]>,
        depth: u32,
    ) -> bool {
        let held = self.layers.get_key_value(layer);
        let record = self.store.get(layer, level, ordinal);
        let manifest = &self.resolver.generation.bundle.manifest;
        let views = match (held, record, &self.views) {
            (Some((_, (_, declaration))), Some(record), Some(reached)) => declaration
                .views
                .iter()
                .map(String::as_str)
                .filter(|view| within.is_none_or(|within| within.contains(view)))
                .filter(|view| reached.contains_view(view))
                .filter(|view| {
                    manifest.incarnation_of(view).is_some_and(|incarnation| {
                        record.drawn_in(crate::artifacts::view_key(view), incarnation)
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        let facts = Reached {
            reach: self,
            held: held.map(|(name, (space, declaration))| (name.as_str(), *space, declaration)),
            level,
            ordinal,
            record,
            views,
            depth,
        };
        let criterion =
            held.and_then(|(_, (_, declaration))| declaration.require_member_visibility);
        crate::artifacts::verdict(criterion, &facts).is_served()
    }

    /// Whether the writer may name the artifact `entity` of `space` that the store holds, or the
    /// layer itself where `entity` is the layer's own.
    pub(crate) fn target(
        &self,
        registry: &mosaica_lifecycle::LayerRegistry,
        space: LayerSpace,
        entity: EntityId,
    ) -> bool {
        use mosaica_lifecycle::Reach;
        let Some((name, _)) = registry.in_space(space) else {
            return false;
        };
        if entity == mosaica_types::layer::LAYER_ENTITY {
            return self.layer(name);
        }
        registry
            .locate_in(space, entity)
            .is_some_and(|(name, level, ordinal)| {
                self.store.get(name, level, ordinal).is_some()
                    && self.artifact(name, level, ordinal)
            })
    }
}

impl mosaica_lifecycle::Reach for ArtifactReach<'_> {
    fn layer(&self, layer: &str) -> bool {
        matches!(self.resolver.scope, WriteScope::All) || self.layers.contains_key(layer)
    }

    fn artifact(&self, layer: &str, level: u32, ordinal: u32) -> bool {
        matches!(self.resolver.scope, WriteScope::All)
            || self.served(
                layer,
                level,
                ordinal,
                None,
                crate::artifacts::DEPENDENCY_CHAIN_MAX,
            )
    }
}

/// One artifact as [`ArtifactReach::served`] puts it to the verdict.
struct Reached<'f, 'r> {
    reach: &'f ArtifactReach<'r>,
    /// The layer's name, space and declaration, where the writer reaches it.
    held: Option<(
        &'f str,
        LayerSpace,
        &'f mosaica_types::layer::LayerDeclaration,
    )>,
    level: u32,
    ordinal: u32,
    record: Option<&'r mosaica_lifecycle::membership::ArtifactRecord>,
    /// The views the writer reaches that the artifact is drawn in.
    views: Vec<&'f str>,
    /// How many more attachments the verdict follows.
    depth: u32,
}

impl crate::artifacts::ArtifactFacts for Reached<'_, '_> {
    fn held(&self) -> bool {
        self.record.is_some() && !self.views.is_empty()
    }

    fn denied(&self) -> bool {
        let (Some((_, space, _)), Some(record)) = (self.held, self.record) else {
            return true;
        };
        self.reach
            .resolver
            .generation
            .space_overlay(space)
            .is_deleted(record.entity)
    }

    fn layer_reached(&self) -> bool {
        self.held.is_some()
    }

    fn admits_label(&self) -> bool {
        let (Some((_, _, declaration)), Some(record)) = (self.held, self.record) else {
            return false;
        };
        self.reach
            .resolver
            .scope
            .admits_artifact_label(declaration, &record.access)
    }

    fn attachment_served(&self) -> bool {
        let Some(attachment) = self.record.and_then(|record| record.attached_to.as_ref()) else {
            return true;
        };
        // The target as the edge names it: its layer under the space it was resolved in, and its
        // slot holding the entity it names.
        let target = self
            .reach
            .layers
            .get(&attachment.layer)
            .filter(|(space, _)| *space == attachment.space)
            .and_then(|_| {
                self.reach
                    .store
                    .get(&attachment.layer, attachment.level, attachment.ordinal)
            })
            .is_some_and(|record| record.entity == attachment.entity);
        self.depth > 0
            && target
            && self.reach.served(
                &attachment.layer,
                attachment.level,
                attachment.ordinal,
                Some(self.views.as_slice()),
                self.depth - 1,
            )
    }

    fn masked_count(&self) -> u64 {
        let Some((layer, ..)) = self.held else {
            return 0;
        };
        let store = self.reach.store;
        store
            .shards()
            .filter_map(|shard| {
                let slice = store.slice(shard)?;
                let record = slice.get(layer, self.level, self.ordinal)?;
                Some(self.reach.within(shard, &slice.members_of(record).whole()))
            })
            .sum()
    }

    fn declared_size(&self) -> u64 {
        self.held.map_or(0, |(layer, ..)| {
            self.reach
                .store
                .declared_size(layer, self.level, self.ordinal)
        })
    }

    fn position(&self) -> Result<Option<u32>, crate::artifacts::Withheld> {
        use crate::artifacts::Containment;
        let (Some((layer, _, declaration)), Some(record)) = (self.held, self.record) else {
            return Err(crate::artifacts::Withheld::Containment);
        };
        let store = self.reach.store;
        let shards: Vec<ShardId> = store.shards().collect();
        let generating = |side: usize, rank: usize| {
            store
                .slice(shards[side])?
                .get(layer, self.level, self.ordinal)?
                .contents
                .get(rank)
                .map(|content| &content.generated_from)
        };
        let declared: Vec<u64> = record.contents.iter().map(|c| c.cardinality).collect();
        let contained = crate::artifacts::satisfied_rank_over(
            &declared,
            !declaration.content.supplied.is_empty(),
            shards.len(),
            |side, rank| generating(side, rank).map_or(0, Bitmap::cardinality),
            |side, rank, size| {
                let held =
                    generating(side, rank).map_or(0, |set| self.reach.within(shards[side], set));
                Some(held == size)
            },
        );
        match contained {
            Some(Containment::NothingToContain) => Ok(None),
            Some(Containment::Satisfied(rank)) => {
                Ok(Some(record.content_position(rank as usize) as u32))
            }
            _ => Err(crate::artifacts::Withheld::Containment),
        }
    }
}

/// What every one of several writers may name; an empty list names everything.
pub(crate) struct ReachOfAll<'r>(pub(crate) Vec<ArtifactReach<'r>>);

impl mosaica_lifecycle::Reach for ReachOfAll<'_> {
    fn layer(&self, layer: &str) -> bool {
        self.0.iter().all(|reach| reach.layer(layer))
    }

    fn artifact(&self, layer: &str, level: u32, ordinal: u32) -> bool {
        self.0
            .iter()
            .all(|reach| reach.artifact(layer, level, ordinal))
    }
}

/// The rows a write refused where it applied, by row of its names, with their reasons.
pub type Refused = Vec<(usize, Reason)>;

/// A table's unique columns, each with its declared position, its declaration and its cells.
type UniqueColumns<'a, 't> = Vec<(u16, &'a DeclaredScalar, &'t [Option<AddressValue>])>;

/// What a resolution decided for the rows it was given, in their order, and the columns it
/// ignored.
pub(crate) struct Resolved {
    pub(crate) verdicts: Vec<Result<Target, Refusal>>,
    pub(crate) ignored: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(
        generation: &'a Generation,
        scope: &'a WriteScope,
        key: &'a IdentityKey,
        live: &'a LiveState,
    ) -> Resolver<'a> {
        Resolver {
            generation,
            scope,
            key,
            live,
        }
    }

    /// Whether the writer may name `item`: always for [`WriteScope::All`], and otherwise where one
    /// of the item's index keys, buffered or flushed, is one its terms satisfy. An item whose keys
    /// cannot be read is outside.
    pub(crate) fn admits(&self, item: ItemAt) -> bool {
        let WriteScope::Terms(scope, _) = self.scope else {
            return true;
        };
        let (generation, live) = (self.generation, self.live);
        let Some(shard) = generation.shard(item.shard) else {
            return false;
        };
        match shard.buffer.get(item.entity) {
            Some(buffered) => scope.admits_keys(&buffered.terms, generation, live),
            None => crate::write::joined::flushed_terms_of(shard, item.entity)
                .is_some_and(|keys| scope.admits_keys(&keys, generation, live)),
        }
    }

    /// `set`, entities of `shard`, less those outside the writer's scope: the flushed ones by the
    /// shard's fragment in the scope's masks, where it holds one built at the shard's identity, and
    /// the rest one at a time.
    pub(crate) fn mask(&self, shard: ShardId, set: &mut Bitmap) {
        let WriteScope::Terms(_, masks) = self.scope else {
            return;
        };
        let identity = self.generation.shard(shard).map(|held| held.identity);
        let fragment = masks
            .0
            .iter()
            .find(|(id, fragment)| *id == shard && Some(fragment.identity) == identity);
        let (mut kept, alone) = match fragment {
            // A fragment holds the flushed entities below its watermark; a buffered item's entity
            // is at or past it.
            Some((_, fragment)) => {
                let mut alone = set.clone();
                alone.remove_range(0..u32::try_from(fragment.watermark).unwrap_or(u32::MAX));
                (fragment.view().and(set), alone)
            }
            None => (Bitmap::new(), std::mem::take(set)),
        };
        for entity in alone.iter() {
            if self.admits(ItemAt::new(shard, EntityId::new(u64::from(entity)))) {
                kept.add(entity);
            }
        }
        *set = kept;
    }

    /// The item each of `ids` names, `None` for one that names no item, an artifact or a layer
    /// among them. An identifier names a number in its shard, which names the entity the shard's
    /// edited-items map holds for it ([`crate::edited`]). The item must be below the shard's high
    /// water, held ([`crate::edited::holds_item`]), not deleted (a fold drops a deletion with the
    /// item, so the overlay alone does not say it is gone), and inside the writer's scope.
    pub(crate) fn mosaica_items(
        &self,
        ids: &[MosaicaId],
    ) -> Result<Vec<Option<ItemAt>>, StoreError> {
        let by_shard = mosaica_types::by_shard(ids.iter().enumerate().filter_map(|(at, id)| {
            let (shard, number) = crate::edited::number_named(self.key, self.generation, *id)?;
            Some((shard.id(), (at, shard, number)))
        }));
        let mut out = vec![None; ids.len()];
        for (id, named) in by_shard {
            let shard = named[0].1;
            let high_water = self.live.shard(id).map_or(0, |shard| shard.high_water());
            let numbers: Vec<EntityId> = named.iter().map(|(_, _, number)| *number).collect();
            let held = crate::edited::entities_of_numbers(shard, &numbers, |entity| {
                entity.raw() < high_water
                    && crate::edited::holds_item(shard, entity)
                    && !shard.overlay.is_deleted(entity)
            })?;
            for ((at, ..), entity) in named.iter().zip(held) {
                out[*at] = entity
                    .map(|entity| ItemAt::new(id, entity))
                    .filter(|item| self.admits(*item));
            }
        }
        Ok(out)
    }

    /// What each of `artifacts` names: the layer itself, where the entity is its space's own, or
    /// an artifact its level holds that the generation has not deleted, where the writer may name
    /// it ([`ArtifactReach`]).
    pub(crate) fn artifacts(&self, artifacts: &[(LayerSpace, EntityId)]) -> Vec<Option<Target>> {
        if artifacts.is_empty() {
            return Vec::new();
        }
        self.live.with_layers_and_artifacts(|registry, store| {
            let reach = ArtifactReach::new(self, registry, store);
            artifacts
                .iter()
                .map(|&(space, entity)| {
                    let named = !self.generation.space_overlay(space).is_deleted(entity)
                        && reach.target(registry, space, entity);
                    named.then_some(Target::Artifact(space, entity))
                })
                .collect()
        })
    }

    /// The unique columns of `table` this generation holds an index of, with their cells, and the
    /// other columns, which name nothing.
    fn columns<'t>(&self, table: &'t AddressTable) -> (UniqueColumns<'a, 't>, Vec<String>) {
        let declared = &self.generation.bundle.manifest.declared_scalars;
        let mut unique = Vec::new();
        let mut ignored = Vec::new();
        for (name, cells) in &table.columns {
            let found = declared
                .iter()
                .enumerate()
                .find(|(_, d)| d.name == *name)
                .filter(|_| {
                    self.generation
                        .shards()
                        .any(|shard| shard.unique.get(name).is_some())
                });
            match found {
                Some((at, d)) => unique.push((at as u16, d, cells.as_slice())),
                None => ignored.push(name.clone()),
            }
        }
        (unique, ignored)
    }

    /// Which item each of `rows` of `table` names, every row where `rows` is `None`, under the
    /// identity rule ([`mosaica_lifecycle::resolve`]). A column that is neither `mosaica_id` nor a
    /// unique field is ignored. Refused with [`EngineError::AddressMalformed`] where a table with
    /// rows has no column to name items by, or a value is not one of its field's.
    pub(crate) fn items(
        &self,
        table: &AddressTable,
        rows: Option<&[usize]>,
    ) -> Result<Resolved, EngineError> {
        let (unique, ignored) = self.columns(table);
        let picked: Vec<usize> = match rows {
            Some(rows) => rows.to_vec(),
            None => (0..table.rows).collect(),
        };
        if picked.is_empty() {
            return Ok(Resolved {
                verdicts: Vec::new(),
                ignored,
            });
        }
        if rows.is_none() {
            resolve::require_identifier(table.mosaica_id.is_some(), unique.len()).map_err(|e| {
                EngineError::AddressMalformed(format!("This request cannot address items: {e}"))
            })?;
        }
        let store = EngineError::Store;
        let no_item = |row| Verdict::Refused(Refusal::NamesNoItem { row });

        // One column alone, the common case, is decided without a row's identity per row.
        let verdicts = match (&table.mosaica_id, unique.as_slice()) {
            (Some(ids), []) => {
                let present: Vec<(usize, MosaicaId)> = picked
                    .iter()
                    .enumerate()
                    .filter_map(|(at, row)| ids[*row].map(|id| (at, id)))
                    .collect();
                let wanted: Vec<MosaicaId> = present.iter().map(|(_, id)| *id).collect();
                let mut verdicts: Vec<Verdict> = (0..picked.len()).map(no_item).collect();
                let found = self.mosaica_items(&wanted).map_err(store)?;
                for ((at, _), item) in present.iter().zip(found) {
                    verdicts[*at] = match item {
                        Some(item) => Verdict::Names(item),
                        None => Verdict::Refused(Refusal::UnknownMosaicaId { row: *at }),
                    };
                }
                verdicts
            }
            (None, [(position, d, cells)]) => {
                let mut keys = Vec::with_capacity(picked.len());
                let mut from = Vec::with_capacity(picked.len());
                for (at, row) in picked.iter().enumerate() {
                    let Some(value) = &cells[*row] else { continue };
                    if let Some(key) = key_of_value(d, value)? {
                        keys.push(key);
                        from.push(at);
                    }
                }
                let mut verdicts: Vec<Verdict> = (0..picked.len()).map(no_item).collect();
                let field = resolve::Identifier::Unique(*position);
                for (at, holder) in self.holders_of(d, &keys).map_err(store)? {
                    let Holder::Inside(item) = holder else {
                        continue;
                    };
                    let row = from[at];
                    verdicts[row] = match &verdicts[row] {
                        Verdict::Names(held) if *held != item => {
                            Verdict::Refused(Refusal::NamesTwo {
                                row,
                                named: vec![(field, *held), (field, item)],
                            })
                        }
                        Verdict::Refused(Refusal::NamesTwo { .. }) => continue,
                        _ => Verdict::Names(item),
                    };
                }
                verdicts
            }
            _ => {
                let mut identities = vec![RowIdentity::default(); picked.len()];
                if let Some(ids) = &table.mosaica_id {
                    for (identity, row) in identities.iter_mut().zip(&picked) {
                        identity.mosaica_id = ids[*row];
                    }
                }
                for (position, d, cells) in &unique {
                    for (identity, row) in identities.iter_mut().zip(&picked) {
                        let Some(value) = &cells[*row] else { continue };
                        if let Some(key) = key_of_value(d, value)? {
                            identity.unique.push((*position, key.widen()));
                        }
                    }
                }
                resolve::resolve(&identities, self, Batch::Names).map_err(store)?
            }
        };
        let verdicts = verdicts
            .into_iter()
            .zip(&picked)
            .map(|(verdict, row)| match verdict {
                Verdict::Names(item) => Ok(item.target()),
                Verdict::Refused(refusal) => Err(at_row(refusal, *row)),
                Verdict::Creates => unreachable!("a call that addresses items creates none"),
            })
            .collect();
        Ok(Resolved { verdicts, ignored })
    }

    /// [`Self::items`] for a call that may also address an artifact or a layer by its
    /// `mosaica_id`, as `/control/changes` does. A row whose `mosaica_id` names an artifact and
    /// whose unique values name an item names two things, and is refused.
    pub(crate) fn targets(
        &self,
        table: &AddressTable,
        rows: Option<&[usize]>,
    ) -> Result<Resolved, EngineError> {
        let picked: Vec<usize> = match rows {
            Some(rows) => rows.to_vec(),
            None => (0..table.rows).collect(),
        };
        let artifact_rows: Vec<(usize, LayerSpace, EntityId)> = picked
            .iter()
            .enumerate()
            .filter_map(|(at, row)| {
                let id = table.mosaica_id.as_ref()?[*row]?;
                let (space, entity) = self.key.invert_artifact(id)?;
                Some((at, space, entity))
            })
            .collect();
        if artifact_rows.is_empty() {
            return self.items(table, rows);
        }
        let mut items = table.clone();
        if let Some(ids) = items.mosaica_id.as_mut() {
            for (at, ..) in &artifact_rows {
                ids[picked[*at]] = None;
            }
        }
        let mut resolved = self.items(&items, rows)?;
        let named: Vec<(LayerSpace, EntityId)> = artifact_rows
            .iter()
            .map(|&(_, space, entity)| (space, entity))
            .collect();
        for (&(at, ..), artifact) in artifact_rows.iter().zip(self.artifacts(&named)) {
            let row = picked[at];
            resolved.verdicts[at] = match (&resolved.verdicts[at], artifact) {
                (Ok(_), _) => Err(Refusal::NamesTwo {
                    row,
                    named: Vec::new(),
                }),
                (Err(_), Some(target)) => Ok(target),
                (Err(_), None) => Err(Refusal::UnknownMosaicaId { row }),
            };
        }
        Ok(resolved)
    }

    /// The items holding each of `keys` in unique column `declared`, each inside or outside the
    /// writer's scope.
    fn holders_of(
        &self,
        declared: &DeclaredScalar,
        keys: &[UniqueKey],
    ) -> Result<Vec<(usize, Holder)>, StoreError> {
        let held = crate::unique::holders(self.generation, &declared.name, keys)?;
        Ok(held
            .into_iter()
            .map(|(at, item)| match self.admits(item) {
                true => (at, Holder::Inside(item)),
                false => (at, Holder::Outside),
            })
            .collect())
    }
}

impl resolve::Holdings for Resolver<'_> {
    type Error = StoreError;

    fn holders(
        &self,
        field: u16,
        keys: &[resolve::Key],
    ) -> Result<Vec<(usize, Holder)>, StoreError> {
        let declared = &self.generation.bundle.manifest.declared_scalars[usize::from(field)];
        let keys = crate::unique::keys_of_widened(declared, keys.iter().copied())
            .expect("a unique column's type takes a key");
        self.holders_of(declared, &keys)
    }

    fn mosaica_holders(&self, ids: &[MosaicaId]) -> Result<Vec<Option<ItemAt>>, StoreError> {
        self.mosaica_items(ids)
    }
}

/// `refusal` as row `row` of the whole table refuses it.
fn at_row(refusal: Refusal, row: usize) -> Refusal {
    match refusal {
        Refusal::NamesTwo { named, .. } => Refusal::NamesTwo { row, named },
        Refusal::UnknownMosaicaId { .. } => Refusal::UnknownMosaicaId { row },
        Refusal::NamesNoItem { .. } => Refusal::NamesNoItem { row },
        Refusal::ValueHeld { field, creates, .. } => Refusal::ValueHeld {
            row,
            field,
            creates,
        },
        Refusal::LabelNotHeld { .. } => Refusal::LabelNotHeld { row },
        other => other,
    }
}

/// A unique value as a caller wrote it, as its field's index keys it.
pub(crate) fn key_of_value(
    declared: &DeclaredScalar,
    value: &AddressValue,
) -> Result<Option<UniqueKey>, EngineError> {
    let keyword = declared.arrow_type == mosaica_spatial::tiler::ScalarType::Keyword;
    let integer = match (value, keyword) {
        (AddressValue::Text(text), true) => return Ok(Some(UniqueKey::keyword(text))),
        (AddressValue::Integer(n), true) => {
            return Err(EngineError::AddressMalformed(format!(
                "{n} is not a value of '{}', which holds keywords. Send it as a string",
                declared.name
            )))
        }
        (AddressValue::Other(shown), _) => {
            return Err(EngineError::AddressMalformed(format!(
                "{shown} is not a value of '{}'. Send {}",
                declared.name,
                match keyword {
                    true => "a string",
                    false => "a whole number",
                }
            )))
        }
        (AddressValue::Integer(n), false) => *n,
        (AddressValue::Text(text), false) => text.parse().map_err(|_| {
            EngineError::AddressMalformed(format!(
                "'{text}' is not a value of '{}', which holds integers. Write it in decimal digits",
                declared.name
            ))
        })?,
    };
    Ok(mosaica_store::unique::key_of_integer(
        declared.arrow_type,
        integer,
    ))
}

/// A unique value of one field, as [`Names`] looks it up.
fn value_hash(field: u16, key: u128) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    (field, key).hash(&mut hasher);
    hasher.finish()
}

/// A write's names: what each row of its table named in the generation its handler resolved it
/// in, and what the write thread needs to resolve again the rows something has touched since.
#[derive(Debug, Clone)]
pub struct Names {
    pub(crate) scope: WriteScope,
    stamp: Stamp,
    table: AddressTable,
    /// Whether a row may name an artifact or a layer by its `mosaica_id`, as a change may.
    targets: bool,
    ignored: Vec<String>,
    verdicts: Vec<Option<Target>>,
    /// The refused rows, ascending.
    refused: Vec<(usize, Refusal)>,
    /// Every item a row's identifiers named, a refused row's among them.
    named: ShardSets,
    /// The same items, each with a row naming it, ascending.
    by_item: Vec<(ItemAt, usize)>,
    /// Each unique value a row carries, as `(value_hash, row)`, ascending.
    valued: Vec<(u64, usize)>,
}

/// A row [`Names::now`] resolved again, and what it named before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Moved {
    pub(crate) row: usize,
    pub(crate) was: Option<Target>,
}

impl Names {
    /// `table`'s rows resolved by `resolver`, which may name artifacts and layers where `targets`.
    pub(crate) fn resolve(
        resolver: &Resolver,
        table: AddressTable,
        targets: bool,
    ) -> Result<Names, EngineError> {
        let resolved = match targets {
            true => resolver.targets(&table, None)?,
            false => resolver.items(&table, None)?,
        };
        let mut valued = Vec::new();
        let (unique, _) = resolver.columns(&table);
        for (position, d, cells) in &unique {
            for (row, cell) in cells.iter().enumerate() {
                let key = match cell {
                    Some(value) => key_of_value(d, value)?,
                    None => None,
                };
                if let Some(key) = key {
                    valued.push((value_hash(*position, key.widen()), row));
                }
            }
        }
        valued.sort_unstable();
        let mut names = Names::unresolved(
            resolver.scope.clone(),
            Stamp::of(resolver.generation),
            table,
            targets,
            valued,
        );
        names.ignored = resolved.ignored;
        names.take((0..names.table.rows).zip(resolved.verdicts));
        Ok(names)
    }

    /// Names for `targets` as resolved at `stamp`, each item addressed by the `mosaica_id` it was
    /// named by: what a caller that resolved entities itself submits.
    pub(crate) fn of_targets(stamp: Stamp, scope: WriteScope, targets: &[NamedTarget]) -> Names {
        let table = AddressTable {
            rows: targets.len(),
            mosaica_id: Some(targets.iter().map(|named| named.item).collect()),
            columns: Vec::new(),
        };
        let mut names = Names::unresolved(scope, stamp, table, true, Vec::new());
        names.take(targets.iter().map(|named| Ok(named.target)).enumerate());
        names
    }

    /// Names of `table` with no row resolved yet.
    fn unresolved(
        scope: WriteScope,
        stamp: Stamp,
        table: AddressTable,
        targets: bool,
        valued: Vec<(u64, usize)>,
    ) -> Names {
        Names {
            scope,
            stamp,
            table,
            targets,
            ignored: Vec::new(),
            verdicts: Vec::new(),
            refused: Vec::new(),
            named: ShardSets::default(),
            by_item: Vec::new(),
            valued,
        }
    }

    /// Set every row's verdict, and gather the items they name.
    fn take(&mut self, verdicts: impl IntoIterator<Item = (usize, Result<Target, Refusal>)>) {
        for (row, verdict) in verdicts {
            if row >= self.verdicts.len() {
                self.verdicts.resize(row + 1, None);
            }
            match verdict {
                Ok(target) => {
                    if let Target::Item(shard, entity) = target {
                        self.by_item.push((ItemAt::new(shard, entity), row));
                    }
                    self.verdicts[row] = Some(target);
                }
                Err(refusal) => {
                    if let Refusal::NamesTwo { named, .. } = &refusal {
                        self.by_item
                            .extend(named.iter().map(|(_, item)| (*item, row)));
                    }
                    self.verdicts[row] = None;
                    self.refused.push((row, refusal));
                }
            }
        }
        self.refused.sort_by_key(|(row, _)| *row);
        self.by_item.sort_unstable();
        self.by_item.dedup();
        self.named = ShardSets::of_items(self.by_item.iter().map(|(item, _)| *item));
    }

    /// The rows naming `item` by any identifier, as resolved.
    fn rows_naming(&self, item: ItemAt) -> impl Iterator<Item = usize> + '_ {
        let from = self.by_item.partition_point(|(held, _)| *held < item);
        self.by_item[from..]
            .iter()
            .take_while(move |(held, _)| *held == item)
            .map(|(_, row)| *row)
    }

    /// What row `row` names, or why it names nothing.
    pub fn verdict(&self, row: usize) -> Result<Target, Reason> {
        match self.verdicts[row] {
            Some(target) => Ok(target),
            None => Err(self.refusal(row).map_or(Reason::NamesNoItem, Refusal::kind)),
        }
    }

    fn refusal(&self, row: usize) -> Option<&Refusal> {
        let at = self.refused.binary_search_by_key(&row, |(r, _)| *r).ok()?;
        Some(&self.refused[at].1)
    }

    /// The refused rows, ascending, with their reasons.
    pub fn refused(&self) -> Refused {
        self.refused
            .iter()
            .map(|(row, refusal)| (*row, refusal.kind()))
            .collect()
    }

    /// The columns that name nothing.
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    pub(crate) fn scope(&self) -> &WriteScope {
        &self.scope
    }

    /// The items rows `rows` name, as resolved.
    fn items_of(&self, rows: Range<usize>) -> impl Iterator<Item = ItemAt> + '_ {
        self.verdicts[rows]
            .iter()
            .filter_map(|verdict| match verdict {
                Some(Target::Item(shard, entity)) => Some(ItemAt::new(*shard, *entity)),
                _ => None,
            })
    }

    /// Bring the names to `generation`, the one the write applies in, and answer the rows resolved
    /// again, ascending, with what each named before. A row is resolved again where an item it
    /// names has been deleted or folded away since, or a value it carries has gained a holder;
    /// every item row is where a fold since is no longer kept or the unique columns have changed.
    /// Called once, where the write applies.
    pub(crate) fn now(
        &mut self,
        generation: &Generation,
        key: &IdentityKey,
        live: &LiveState,
    ) -> Result<Vec<Moved>, EngineError> {
        let scope = self.scope.clone();
        let at = &Resolver::new(generation, &scope, key, live);
        let unique = &generation.unique_live;
        let retired = generation
            .retirements
            .since(self.stamp.folds, generation.fold_epoch);
        let all = match retired {
            Some(_) => unique.columns_changed_since(self.stamp.unique_seq),
            None => true,
        };
        let mut rows: Vec<usize> = match retired {
            Some(retired) if !all => {
                let mut rows = self.touched(generation, &retired);
                rows.extend(self.valued_since(at)?);
                rows
            }
            _ => (0..self.table.rows).collect(),
        };
        // An artifact or a layer never moves, and keeps its entity for ever.
        rows.retain(|row| !matches!(self.verdicts[*row], Some(Target::Artifact(..))));
        rows.sort_unstable();
        rows.dedup();
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let resolved = match self.targets {
            true => at.targets(&self.table, Some(&rows))?,
            false => at.items(&self.table, Some(&rows))?,
        };
        self.refused
            .retain(|(row, _)| rows.binary_search(row).is_err());
        let mut moved = Vec::with_capacity(rows.len());
        for (row, verdict) in rows.into_iter().zip(resolved.verdicts) {
            moved.push(Moved {
                row,
                was: self.verdicts[row],
            });
            match verdict {
                Ok(target) => self.verdicts[row] = Some(target),
                Err(refusal) => {
                    self.verdicts[row] = None;
                    self.refused.push((row, refusal));
                }
            }
        }
        self.refused.sort_by_key(|(row, _)| *row);
        Ok(moved)
    }

    /// The rows naming an item `generation` has deleted, a fold of `retired` has removed, or
    /// that lies in a shard `generation` no longer holds.
    fn touched(&self, generation: &Generation, retired: &[(ShardId, &Bitmap)]) -> Vec<usize> {
        let mut gone: Vec<(ShardId, Bitmap)> = Vec::new();
        for (id, set) in self.named.iter() {
            let hit = match generation.shard(id) {
                None => set.clone(),
                Some(shard) => {
                    let mut hit = set.and(shard.overlay.deleted_set());
                    for (_, removed) in retired.iter().filter(|(shard, _)| *shard == id) {
                        hit.or_inplace(&set.and(removed));
                    }
                    hit
                }
            };
            if !hit.is_empty() {
                gone.push((id, hit));
            }
        }
        gone.iter()
            .flat_map(|(id, hit)| {
                hit.iter().flat_map(move |entity| {
                    self.rows_naming(ItemAt::new(*id, EntityId::new(u64::from(entity))))
                })
            })
            .collect()
    }

    /// The rows carrying a value given a holder since the names were resolved: every row
    /// carrying a value where an entry added since has left memory.
    fn valued_since(&self, at: &Resolver) -> Result<Vec<usize>, EngineError> {
        let live = &at.generation.unique_live;
        if self.valued.is_empty() || live.seq() <= self.stamp.unique_seq {
            return Ok(Vec::new());
        }
        if live.stale_since(self.stamp.unique_seq) {
            return Ok(self.valued.iter().map(|(_, row)| *row).collect());
        }
        let mut rows = Vec::new();
        let (unique, _) = at.columns(&self.table);
        for (position, d, _) in unique {
            for key in live.keys_added_since(&d.name, self.stamp.unique_seq) {
                let hash = value_hash(position, key.widen());
                let from = self.valued.partition_point(|(h, _)| *h < hash);
                rows.extend(
                    self.valued[from..]
                        .iter()
                        .take_while(|(h, _)| *h == hash)
                        .map(|(_, row)| *row),
                );
            }
        }
        Ok(rows)
    }
}

/// Which set of an artifact a member table fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSet {
    /// A publication's `members`, or a growth's joining members.
    Members,
    Excluding,
    /// The generating set of the publication's content at this position in its contents.
    GeneratedFrom(usize),
    /// A growth's leaving members.
    Leaving,
}

impl MemberSet {
    /// Whether a refused row of this set refuses the write: one of a content's generating set,
    /// which a viewer must see whole to be served the content, or of members joining one, where
    /// the growth is `ranked`.
    fn whole(self, ranked: bool) -> bool {
        match self {
            MemberSet::GeneratedFrom(_) => true,
            MemberSet::Members => ranked,
            MemberSet::Excluding | MemberSet::Leaving => false,
        }
    }
}

/// One member table of a write, as the rows of its [`Names`] it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedList {
    /// The position of the artifact the table belongs to in the write.
    pub artifact: usize,
    pub set: MemberSet,
    pub rows: Range<usize>,
}

/// A publication's or a growth's member tables, resolved as one table.
#[derive(Debug, Clone)]
pub struct MemberNames {
    pub(crate) names: Names,
    pub(crate) lists: Vec<NamedList>,
    /// A refused row, or a refused artifact, refuses the write.
    pub(crate) strict: bool,
}

impl MemberNames {
    pub fn new(names: Names, lists: Vec<NamedList>, strict: bool) -> MemberNames {
        MemberNames {
            names,
            lists,
            strict,
        }
    }

    pub fn names(&self) -> &Names {
        &self.names
    }

    /// The set `list` fills: built from its rows where `moved` is `None`, and otherwise `held`,
    /// the set built before, less each item a moved row no longer names and no other row of the
    /// list names, and with each item a moved row names now.
    fn set_of(&self, list: &NamedList, moved: Option<&[Moved]>, held: &mut ShardSets) {
        let Some(moved) = moved else {
            *held = ShardSets::of_items(self.names.items_of(list.rows.clone()));
            return;
        };
        let from = moved.partition_point(|m| m.row < list.rows.start);
        let within: Vec<&Moved> = moved[from..]
            .iter()
            .take_while(|m| m.row < list.rows.end)
            .collect();
        if within.is_empty() {
            return;
        }
        let moved_row = |row: usize| within.binary_search_by_key(&row, |m| m.row).is_ok();
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for m in &within {
            if let Some(Target::Item(shard, entity)) = m.was {
                let item = ItemAt::new(shard, entity);
                let kept = self.names.rows_naming(item).any(|row| {
                    list.rows.contains(&row)
                        && !moved_row(row)
                        && self.names.verdicts[row] == Some(item.target())
                });
                if !kept {
                    removed.push(item);
                }
            }
            if let Ok(Target::Item(shard, entity)) = self.names.verdict(m.row) {
                added.push(ItemAt::new(shard, entity));
            }
        }
        let shards: std::collections::BTreeSet<ShardId> = removed
            .iter()
            .chain(&added)
            .map(|item| item.shard)
            .collect();
        for shard in shards {
            let mut set = held.get(shard).cloned().unwrap_or_default();
            for item in removed.iter().filter(|item| item.shard == shard) {
                set.remove(item.entity.raw() as u32);
            }
            for item in added.iter().filter(|item| item.shard == shard) {
                set.add(item.entity.raw() as u32);
            }
            held.set(shard, set);
        }
    }

    /// Each list's set, as [`Self::set_of`] makes it.
    pub(crate) fn fill_artifacts(
        &self,
        artifacts: &mut [IncomingArtifact],
        moved: Option<&[Moved]>,
    ) {
        for list in &self.lists {
            let Some(artifact) = artifacts.get_mut(list.artifact) else {
                continue;
            };
            match list.set {
                MemberSet::Members => self.set_of(list, moved, &mut artifact.members),
                MemberSet::Excluding => self.set_of(
                    list,
                    moved,
                    artifact.excluding.get_or_insert_with(Default::default),
                ),
                MemberSet::GeneratedFrom(at) => {
                    if let Some(content) = artifact.contents.get_mut(at) {
                        self.set_of(list, moved, &mut content.generated_from);
                    }
                }
                MemberSet::Leaving => {}
            }
        }
    }

    /// [`Self::fill_artifacts`] for a growth's joins.
    pub(crate) fn fill_growth(&self, joins: &mut [IncomingGrowth], moved: Option<&[Moved]>) {
        for list in &self.lists {
            let Some(join) = joins.get_mut(list.artifact) else {
                continue;
            };
            match list.set {
                MemberSet::Members => self.set_of(list, moved, &mut join.joining),
                MemberSet::Leaving => self.set_of(list, moved, &mut join.leaving),
                MemberSet::Excluding | MemberSet::GeneratedFrom(_) => {}
            }
        }
    }

    /// The first refused row that refuses the write, with its reason: any, where the write is
    /// strict, and otherwise one of a set a refused row refuses whole, `ranked` saying which
    /// artifacts are a ranked growth's.
    pub(crate) fn refusing(&self, ranked: impl Fn(usize) -> bool) -> Option<(usize, Reason)> {
        self.names
            .refused
            .iter()
            .find(|(row, _)| {
                self.lists.iter().any(|list| {
                    list.rows.contains(row)
                        && (self.strict || list.set.whole(ranked(list.artifact)))
                })
            })
            .map(|(row, refusal)| (*row, refusal.kind()))
    }

    /// Names for the members of each set `lists` yields, as `generation` holds them, each
    /// addressed by its `mosaica_id`: what a caller that resolved entities itself submits.
    pub(crate) fn of_sets<'s>(
        key: &IdentityKey,
        generation: &Generation,
        lists: impl IntoIterator<Item = (usize, MemberSet, &'s ShardSets)>,
    ) -> Result<MemberNames, StoreError> {
        let mut targets = Vec::new();
        let mut named = Vec::new();
        for (artifact, set, sets) in lists {
            let start = targets.len();
            for (shard, members) in sets.iter() {
                targets.extend(
                    members
                        .iter()
                        .map(|e| Target::Item(shard, EntityId::new(u64::from(e)))),
                );
            }
            named.push(NamedList {
                artifact,
                set,
                rows: start..targets.len(),
            });
        }
        let resolved = NamedTarget::all(key, generation, &targets)?;
        let names = Names::of_targets(Stamp::of(generation), WriteScope::All, &resolved);
        Ok(MemberNames::new(names, named, false))
    }
}

/// The entities the latest folds removed, by the fold epoch each published at: how the write
/// thread finds the items a fold has removed since a write's names were resolved.
#[derive(Debug, Clone, Default)]
pub(crate) struct Retirements(VecDeque<(u64, ShardId, Arc<Bitmap>)>);

/// How many folds' removals are kept. A write whose names were resolved before the oldest kept
/// one has every row resolved again.
const RETIREMENTS_KEPT: usize = 16;

impl Retirements {
    /// These, and the entities of `shard` the fold publishing at `epoch` removed.
    pub(crate) fn with(&self, epoch: u64, shard: ShardId, retired: &Bitmap) -> Retirements {
        let mut kept = self.0.clone();
        kept.push_back((epoch, shard, Arc::new(retired.clone())));
        while kept.len() > RETIREMENTS_KEPT {
            kept.pop_front();
        }
        Retirements(kept)
    }

    /// What every fold after epoch `since`, through `now`, removed, or `None` where one of them is
    /// no longer kept.
    pub(crate) fn since(&self, since: u64, now: u64) -> Option<Vec<(ShardId, &Bitmap)>> {
        let after: Vec<(ShardId, &Bitmap)> = self
            .0
            .iter()
            .filter(|(epoch, ..)| *epoch > since)
            .map(|(_, shard, retired)| (*shard, retired.as_ref()))
            .collect();
        (after.len() as u64 == now.saturating_sub(since)).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(entities: &[u32]) -> Bitmap {
        entities.iter().copied().collect()
    }

    #[test]
    fn a_fold_since_a_write_was_named_is_answered_while_it_is_kept() {
        let mut kept = Retirements::default();
        assert_eq!(kept.since(0, 0).map(|r| r.len()), Some(0), "no fold since");
        for epoch in 1..=RETIREMENTS_KEPT as u64 + 2 {
            kept = kept.with(epoch, ShardId::ZERO, &bitmap(&[epoch as u32]));
        }
        let now = RETIREMENTS_KEPT as u64 + 2;
        let since = kept.since(now - 1, now).expect("the last fold is kept");
        assert_eq!(since.len(), 1);
        assert!(since[0].1.contains(now as u32));
        assert_eq!(kept.since(now, now).map(|r| r.len()), Some(0));
        assert!(
            kept.since(1, now).is_none(),
            "the fold after epoch 1 is no longer kept"
        );
        assert_eq!(
            kept.since(2, now).map(|r| r.len()),
            Some(RETIREMENTS_KEPT),
            "every kept fold"
        );
    }
}
