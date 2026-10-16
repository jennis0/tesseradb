//! The control-plane calls: resolving, writing, declaring and publishing.

use std::sync::atomic::Ordering;

use mosaica_lifecycle::resolve::{self, Batch, RowIdentity, Verdict};
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_store::manifest::DeclaredScalar;
use mosaica_store::unique::{KeyKind, UniqueKey};
use mosaica_store::StoreError;
use mosaica_types::{EntityId, ItemAt, LayerSpace, MosaicaId, Target, TermId};

use crate::edited::Stamp;
use crate::engine::Engine;
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

    /// Which item each row of a call that addresses items names, under the identity rule
    /// ([`mosaica_lifecycle::resolve`]): a row names the item its `mosaica_id` and unique values
    /// name, and a row naming none, or naming two, is refused. Many rows may name one item. A
    /// column that is neither `mosaica_id` nor a unique field names nothing and is ignored, as a
    /// build ignores it; the answer names it.
    ///
    /// Refused with [`crate::EngineError::AddressMalformed`] where a table with rows is left with
    /// no column to name items by, or a value is not one of its field's.
    pub fn name_items(
        &self,
        table: &AddressTable,
    ) -> std::result::Result<NamedItems, crate::EngineError> {
        name_items_in(self, &self.generation.load_full(), table)
    }

    /// [`Engine::name_items`] for a call that may also address an artifact or a layer by its
    /// `mosaica_id`, as `/control/changes` does. A row whose `mosaica_id` names an artifact and
    /// whose unique values name an item names two things, and is refused.
    pub fn name_targets(
        &self,
        table: &AddressTable,
    ) -> std::result::Result<NamedTargets, crate::EngineError> {
        let generation = self.generation.load_full();
        let artifact_rows: Vec<(usize, LayerSpace, EntityId)> = table
            .mosaica_id
            .iter()
            .flatten()
            .enumerate()
            .filter_map(|(row, id)| {
                let (space, entity) = self.identity_key.invert_artifact((*id)?)?;
                Some((row, space, entity))
            })
            .collect();
        let mut items = table.clone();
        if let Some(ids) = items.mosaica_id.as_mut() {
            for (row, ..) in &artifact_rows {
                ids[*row] = None;
            }
        }
        let named = name_items_in(self, &generation, &items)?;
        let mut verdicts: Vec<std::result::Result<Target, resolve::Refusal>> = named
            .verdicts
            .into_iter()
            .map(|verdict| verdict.map(ItemAt::target))
            .collect();
        for &(row, space, entity) in &artifact_rows {
            let target = self.artifact_target_in(&generation, space, entity);
            verdicts[row] = match (&verdicts[row], target) {
                (Ok(_), _) => Err(resolve::Refusal::NamesTwo {
                    row,
                    named: Vec::new(),
                }),
                (Err(_), Some(target)) => Ok(target),
                (Err(_), None) => Err(resolve::Refusal::UnknownMosaicaId { row }),
            };
        }
        Ok(NamedTargets {
            verdicts,
            ignored: named.ignored,
            at: named.at,
        })
    }
}

/// What [`Engine::name_targets`] decided: for each row, what it names or why it was refused; the
/// columns it ignored; and the generation it resolved them against.
#[derive(Debug, Clone)]
pub struct NamedTargets {
    pub verdicts: Vec<std::result::Result<Target, resolve::Refusal>>,
    pub ignored: Vec<String>,
    pub at: Stamp,
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

/// What [`Engine::name_items`] decided: one verdict per row, the columns it ignored, and the
/// generation it resolved them against, which a write naming the items carries.
#[derive(Debug, Clone)]
pub struct NamedItems {
    pub verdicts: Vec<std::result::Result<ItemAt, resolve::Refusal>>,
    pub ignored: Vec<String>,
    pub at: Stamp,
}

/// [`Engine::name_items`] against `generation`.
pub(crate) fn name_items_in(
    engine: &Engine,
    generation: &Generation,
    table: &AddressTable,
) -> std::result::Result<NamedItems, crate::EngineError> {
    let declared = &generation.bundle.manifest.declared_scalars;
    let mut unique: Vec<(u16, &DeclaredScalar, &[Option<AddressValue>])> = Vec::new();
    let mut ignored: Vec<String> = Vec::new();
    for (name, cells) in &table.columns {
        let found = declared
            .iter()
            .enumerate()
            .find(|(_, d)| d.name == *name)
            .filter(|_| {
                generation
                    .shards()
                    .any(|shard| shard.unique.get(name).is_some())
            });
        match found {
            Some((at, d)) => unique.push((at as u16, d, cells)),
            None => ignored.push(name.clone()),
        }
    }
    let at = Stamp::of(generation);
    if table.rows == 0 {
        return Ok(NamedItems {
            verdicts: Vec::new(),
            ignored,
            at,
        });
    }
    resolve::require_identifier(table.mosaica_id.is_some(), unique.len()).map_err(|e| {
        crate::EngineError::AddressMalformed(format!("This request cannot address items: {e}"))
    })?;
    let store = crate::EngineError::Store;
    let no_item = |row| Verdict::Refused(resolve::Refusal::NamesNoItem { row });

    // One column alone, the common case, is decided without a row's identity per row.
    let verdicts = match (&table.mosaica_id, unique.as_slice()) {
        (Some(ids), []) => {
            let present: Vec<(usize, MosaicaId)> = ids
                .iter()
                .enumerate()
                .filter_map(|(at, id)| id.map(|id| (at, id)))
                .collect();
            let wanted: Vec<MosaicaId> = present.iter().map(|(_, id)| *id).collect();
            let mut verdicts: Vec<Verdict> = (0..table.rows).map(no_item).collect();
            let found = engine.mosaica_ids_in(generation, &wanted).map_err(store)?;
            for ((at, _), item) in present.iter().zip(found) {
                verdicts[*at] = match item {
                    Some(item) => Verdict::Names(item),
                    None => Verdict::Refused(resolve::Refusal::UnknownMosaicaId { row: *at }),
                };
            }
            verdicts
        }
        (None, [(position, d, cells)]) => {
            let mut keys = Vec::with_capacity(table.rows);
            let mut from = Vec::with_capacity(table.rows);
            for (at, cell) in cells.iter().enumerate() {
                let Some(value) = cell else { continue };
                if let Some(key) = key_of_value(d, value)? {
                    keys.push(key);
                    from.push(at);
                }
            }
            let mut verdicts: Vec<Verdict> = (0..table.rows).map(no_item).collect();
            let field = resolve::Identifier::Unique(*position);
            let holders =
                crate::unique::holders_in_every_shard(generation, &d.name, &keys).map_err(store)?;
            for (at, item) in holders {
                let row = from[at];
                verdicts[row] = match &verdicts[row] {
                    Verdict::Names(held) if *held != item => {
                        Verdict::Refused(resolve::Refusal::NamesTwo {
                            row,
                            named: vec![(field, *held), (field, item)],
                        })
                    }
                    Verdict::Refused(resolve::Refusal::NamesTwo { .. }) => continue,
                    _ => Verdict::Names(item),
                };
            }
            verdicts
        }
        _ => {
            let mut rows = vec![RowIdentity::default(); table.rows];
            if let Some(ids) = &table.mosaica_id {
                for (row, id) in rows.iter_mut().zip(ids) {
                    row.mosaica_id = *id;
                }
            }
            for (position, d, cells) in &unique {
                for (row, cell) in rows.iter_mut().zip(cells.iter()) {
                    let Some(value) = cell else { continue };
                    if let Some(key) = key_of_value(d, value)? {
                        row.unique.push((*position, key.widen()));
                    }
                }
            }
            let holdings = Addressed { engine, generation };
            resolve::resolve(&rows, &holdings, Batch::Names).map_err(store)?
        }
    };
    let verdicts = verdicts
        .into_iter()
        .map(|verdict| match verdict {
            Verdict::Names(item) => Ok(item),
            Verdict::Refused(refusal) => Err(refusal),
            Verdict::Creates => unreachable!("a call that addresses items creates none"),
        })
        .collect();
    Ok(NamedItems {
        verdicts,
        ignored,
        at,
    })
}

/// A unique value as a caller wrote it, as its field's index keys it.
fn key_of_value(
    declared: &DeclaredScalar,
    value: &AddressValue,
) -> std::result::Result<Option<UniqueKey>, crate::EngineError> {
    let keyword = declared.arrow_type == mosaica_spatial::tiler::ScalarType::Keyword;
    let integer = match (value, keyword) {
        (AddressValue::Text(text), true) => return Ok(Some(UniqueKey::keyword(text))),
        (AddressValue::Integer(n), true) => {
            return Err(crate::EngineError::AddressMalformed(format!(
                "{n} is not a value of '{}', which holds keywords. Send it as a string",
                declared.name
            )))
        }
        (AddressValue::Other(shown), _) => {
            return Err(crate::EngineError::AddressMalformed(format!(
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
            crate::EngineError::AddressMalformed(format!(
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

/// Who holds what for a call that addresses items: the unique indexes and their live entries,
/// and every `mosaica_id` [`Engine::mosaica_ids_in`] resolves, deleted items left out.
struct Addressed<'a> {
    engine: &'a Engine,
    generation: &'a Generation,
}

impl resolve::Holdings for Addressed<'_> {
    type Error = StoreError;

    fn holders(
        &self,
        field: u16,
        keys: &[resolve::Key],
    ) -> Result<Vec<(usize, ItemAt)>, StoreError> {
        let declared = &self.generation.bundle.manifest.declared_scalars[usize::from(field)];
        let kind = KeyKind::of(declared.arrow_type).expect("a unique column's type takes a key");
        let keys: Vec<UniqueKey> = keys
            .iter()
            .map(|k| UniqueKey::of_widened(kind, *k))
            .collect();
        crate::unique::holders_in_every_shard(self.generation, &declared.name, &keys)
    }

    fn mosaica_holders(&self, ids: &[MosaicaId]) -> Result<Vec<Option<ItemAt>>, StoreError> {
        self.engine.mosaica_ids_in(self.generation, ids)
    }
}

impl Engine {
    /// The item each of `ids` names in `generation`: `None` per position for an identifier that
    /// names no item, an artifact's and a layer's among them. An identifier names a number in its
    /// shard, and the number names the entity the shard's edited-items map holds for it, or the
    /// number itself where it has none ([`crate::edited`]). An item is named while its shard holds
    /// it ([`holds_item`]) below the shard's high water and has not deleted it: a fold that
    /// removes a deleted item also drops its deletion, so the overlay alone no longer says it is
    /// gone.
    pub(crate) fn mosaica_ids_in(
        &self,
        generation: &Generation,
        ids: &[MosaicaId],
    ) -> std::result::Result<Vec<Option<ItemAt>>, StoreError> {
        let by_shard = mosaica_types::by_shard(ids.iter().enumerate().filter_map(|(at, id)| {
            let (shard, number) = crate::edited::number_named(&self.identity_key, generation, *id)?;
            Some((shard.id(), (at, shard, number)))
        }));
        let mut out = vec![None; ids.len()];
        let live = self.write.live();
        for (id, named) in by_shard {
            let shard = named[0].1;
            let high_water = live.shard(id).map_or(0, |shard| shard.high_water());
            let numbers: Vec<EntityId> = named.iter().map(|(_, _, number)| *number).collect();
            let held = crate::edited::entities_of_numbers(shard, &numbers, |entity| {
                entity.raw() < high_water
                    && holds_item(shard, entity)
                    && !shard.overlay.is_deleted(entity)
            })?;
            for ((at, ..), entity) in named.iter().zip(held) {
                out[*at] = entity.map(|entity| ItemAt::new(id, entity));
            }
        }
        Ok(out)
    }

    /// Resolve `mosaica_id`s of items, artifacts and layers to what each names: `None` per
    /// position for an identifier that names nothing.
    pub fn resolve_targets(
        &self,
        ids: &[MosaicaId],
    ) -> std::result::Result<Vec<Option<Target>>, StoreError> {
        let generation = self.generation.load_full();
        let items = self.mosaica_ids_in(&generation, ids)?;
        Ok(ids
            .iter()
            .zip(items)
            .map(|(id, item)| match item {
                Some(item) => Some(item.target()),
                None => {
                    let (space, entity) = self.identity_key.invert_artifact(*id)?;
                    self.artifact_target_in(&generation, space, entity)
                }
            })
            .collect())
    }

    /// The layer itself, where `entity` is `space`'s own, or an artifact its level holds that
    /// `generation` has not deleted.
    fn artifact_target_in(
        &self,
        generation: &Generation,
        space: LayerSpace,
        entity: EntityId,
    ) -> Option<Target> {
        let deleted = generation.space_overlay(space).is_deleted(entity);
        (self.write.live().artifact_held(space, entity) && !deleted)
            .then_some(Target::Artifact(space, entity))
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

    /// The body hash a batch id was accepted with, if it was: a batch replayed with the same
    /// bytes is reported as a replay. The executor decides the same question again, race-free.
    pub fn accepted_batch(&self, batch_id: &str) -> Option<[u8; 32]> {
        self.write
            .live()
            .accepted_batch(batch_id)
            .map(|(hash, _)| hash)
    }

    /// The wire `mosaica_id` of the item `entity` holds in the one shard served: the permutation of
    /// its number, returned instead of the raw `EntityId`, which never crosses the trust boundary.
    pub fn mosaica_id_of(&self, entity: EntityId) -> std::result::Result<MosaicaId, StoreError> {
        self.mosaica_id_in(self.generation.load_full().only_shard(), entity)
    }

    /// Request a flush. Accepted at any time and executed promptly: the flag pulls the tick's
    /// deadline forward and the doorbell wakes an idle executor. The 202 means "accepted, not yet
    /// done"; the response never waits on the segment write. An operator trigger, rate-decoupled
    /// from ingest. [`Engine::request_flush_publication`] is the same request with its
    /// publication number; this drops the number.
    pub fn request_flush(&self) {
        let _ = self.request_flush_publication();
    }

    /// Request a compaction fold — the trigger `POST /control/compact` will take (reserved,
    /// unbuilt). The same shape as [`Engine::request_flush`], but a fold gets its own thread
    /// rather than the shared pool, and a 202 means minutes to hours of IO still to come. At most
    /// one fold is in flight: a second request while one runs is satisfied by neither.
    ///
    /// Not built yet: the trigger's own gauges and minimum interval.
    pub fn request_fold(&self) {
        self.write
            .health()
            .fold_requested
            .store(true, Ordering::SeqCst);
        self.write.wake();
    }

    /// Refuses every write to a bundle of several shards
    /// ([`crate::write::WritePath::writable`]). A caller asks it before reading a write's body.
    pub fn writable(&self) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.writable()
    }

    /// The stamp of the generation served now: what a write naming entities taken from it, rather
    /// than from [`Engine::name_items`], carries.
    fn stamp_now(&self) -> Stamp {
        Stamp::of(&self.generation.load())
    }

    /// [`Engine::accept_changes`] for one change to the item `entity` of the one shard served.
    pub fn accept_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.writable()?;
        let shard = self.generation.load().only_shard().id();
        self.accept_changes(vec![(Target::Item(shard, entity), op)])
    }

    /// [`Engine::accept_changes_at`] for targets as the generation served now holds them.
    pub fn accept_changes(
        &self,
        changes: Vec<(Target, ChangeOp)>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.accept_changes_at(changes, self.stamp_now())
    }

    /// Apply one `/control/changes` request whose entities were resolved at `at`, and wait for its
    /// receipt. The request is one log record: durable whole or not at all. An `Err` does not mean
    /// nothing happened: where the append or fsync fails, every deletion and suppression is
    /// applied all the same. See `ExecError::Wal`.
    pub fn accept_changes_at(
        &self,
        changes: Vec<(Target, ChangeOp)>,
        at: Stamp,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.submit_changes_at(changes, at)?.wait()
    }

    /// [`Engine::submit_changes_at`] for targets as the generation served now holds them.
    pub fn submit_changes(
        &self,
        changes: Vec<(Target, ChangeOp)>,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        self.submit_changes_at(changes, self.stamp_now())
    }

    /// Enqueue one `/control/changes` request whose entities were resolved at `at`, without
    /// waiting, so requests from several callers reach the deny lane together and share one
    /// fsync. A change naming an entity an edit has moved an item away from since `at` also
    /// reaches the item where it is now; a request stale by [`Stamp`]'s rule is answered with
    /// `ExecError::Stale`. Read `PendingChange::wait` before treating either half's `Err` as
    /// "nothing happened".
    ///
    /// A deletion of a layer's own entity is refused, and the request with it, before anything
    /// is applied: a layer is removed by dropping it.
    pub fn submit_changes_at(
        &self,
        changes: Vec<(Target, ChangeOp)>,
        at: Stamp,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        let layer_deleted = changes.iter().find_map(|(target, op)| match (target, op) {
            (Target::Artifact(space, entity), ChangeOp::Delete) => {
                self.write.live().layer_of_own_entity(*space, *entity)
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
        self.write.submit_changes(changes, at)
    }

    /// One registered layer's declaration, by name, with no gate. Answers what a declaration
    /// says, never what is served: [`Engine::visible_layers`] resolves reachability.
    pub fn registered_layer(&self, name: &str) -> Option<mosaica_types::layer::RegisteredLayer> {
        self.write.live().registered_layer(name)
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

    /// Declare a vocabulary while the service runs. Answers `(existing, added, titles)`.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_vocabulary(
        &self,
        request: mosaica_lifecycle::VocabularyRequest,
    ) -> std::result::Result<(bool, u64, u64), crate::write::AcceptError> {
        self.write.declare_vocabulary(request)
    }

    /// A page of values for a vocabulary that exists. Answers `(added, existing, titles)`.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<mosaica_lifecycle::DeclaredValue>,
    ) -> std::result::Result<(u64, u64, u64), crate::write::AcceptError> {
        self.write.mint_vocabulary_values(vocabulary, values)
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

    /// Drop a view, deleting the items it leaves in no view. Its key may be created again, as a
    /// new incarnation with none of the dropped view's rows or artifacts.
    pub fn drop_view(
        &self,
        group: String,
        key: String,
    ) -> std::result::Result<crate::write::ViewDropped, crate::write::AcceptError> {
        self.write.drop_view(group, key)
    }

    /// [`Engine::put_artifacts_at`] for members as the generation served now holds them.
    pub fn put_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<mosaica_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        self.put_artifacts_at(layer, level, artifacts, self.stamp_now())
    }

    /// Put a batch of artifacts into one level of a layer, returning a `mosaica_id` per artifact
    /// and the batch's counts. A held key is filled; an unheld key is published. The members were
    /// resolved at `at`, and are followed and stale on
    /// [`Engine::submit_changes_at`]'s rule. A deleted member refuses the batch; a suppressed one
    /// is accepted.
    pub fn put_artifacts_at(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<mosaica_lifecycle::IncomingArtifact>,
        at: Stamp,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        let batch = self.write.publish_artifacts(layer, level, artifacts, at)?;
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

    /// [`Engine::grow_memberships_at`] for members as the generation served now holds them.
    pub fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<mosaica_lifecycle::IncomingGrowth>,
    ) -> std::result::Result<Vec<GrownMembership>, crate::write::AcceptError> {
        self.grow_memberships_at(layer, level, joins, self.stamp_now())
    }

    /// Add points to the memberships of artifacts that already exist, each named by its published
    /// key. A suppressed artifact grows like any other and stays suppressed: the key resolves
    /// against the store, not what is served. The members were resolved at `at`, and the member
    /// checks are [`Engine::put_artifacts_at`]'s. A point may also name its artifacts via
    /// `/control/ingest`'s column; this entry point is for correcting points that already exist,
    /// and an unknown key is refused here regardless.
    ///
    /// One [`GrownMembership`] per join. Neither an ordinal nor a membership size crosses the
    /// boundary.
    pub fn grow_memberships_at(
        &self,
        layer: String,
        level: u32,
        joins: Vec<mosaica_lifecycle::IncomingGrowth>,
        at: Stamp,
    ) -> std::result::Result<Vec<GrownMembership>, crate::write::AcceptError> {
        let (space, grown) = self.write.grow_memberships(layer, level, joins, at)?;
        grown
            .into_iter()
            .map(|receipt| {
                Ok(GrownMembership {
                    mosaica_id: self.issued_id(space, receipt.entity)?,
                    joined: receipt.joined,
                    filled: receipt.filled,
                    left: receipt.left,
                    withdrawn: receipt.withdrawn,
                })
            })
            .collect()
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
    /// The rank this page emptied, where it emptied one: the content is withdrawn and the caller
    /// supplies it again.
    pub withdrawn: Option<u16>,
}

/// What [`Engine::put_artifacts`] answers: the identifiers in the caller's order and the batch's
/// counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifacts {
    /// One per artifact in the caller's order: a held artifact's own, or the new one's.
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

/// Whether `shard` holds `entity` as an item: a buffered row, a row in some view, or the label a
/// flush wrote, which an item whose views were all dropped still holds. A fold that removes a
/// deleted item removes all three.
pub(crate) fn holds_item(shard: &crate::Shard, entity: EntityId) -> bool {
    crate::edited::holds(shard, entity) || flushed_terms_of(shard, entity).is_some()
}
