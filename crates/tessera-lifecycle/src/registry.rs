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

use tessera_types::layer::{DeclarationError, EntityRun, LayerDeclaration, ReservedRuns};
use tessera_types::{EntityId, TermId};

use crate::alloc::{AllocError, Allocator};
use crate::wal::WalRecord;

/// A layer as the registry holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredLayer {
    pub declaration: LayerDeclaration,
    /// The layer's own entity. Its only job is to give layer suppression somewhere to land, so
    /// `/control/changes` and the deny lane work on a layer exactly as they work on a point.
    pub entity: EntityId,
    /// One entry per level, in level order; a layer declaring no levels has exactly one, its
    /// level 0.
    pub runs: Vec<ReservedRuns>,
    /// Bumped by any edit that changes who may reach this layer, so a session's cached resolution
    /// is invalidated rather than outliving the gate it was computed from.
    pub version: u64,
}

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
    /// A layer named in `depends_on` is not registered. Refused at create rather than discovered at
    /// the first edge, because an edge's target must exist before the edge (`annotation-write-cycle.md`
    /// §5.0.4) and a dangling dependency is that ordering constraint already broken.
    MissingDependency { layer: String, depends_on: String },
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
            RegistryError::MissingDependency { layer, depends_on } => write!(
                f,
                "{layer} declares depends_on {depends_on}, which is not registered — an edge's \
                 target must exist before the edge"
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
            declaration,
            layer_entity,
            runs,
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
                        declaration: declaration.clone(),
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
    /// principal. That error has been made once already in this codebase, in the slice gate, and
    /// was caught in review.
    pub fn resolve_for(
        &self,
        satisfied: &[TermId],
        resolve_label: impl Fn(&str) -> Option<TermId>,
    ) -> ResolvedLayers {
        let names = self
            .layers
            .iter()
            .filter(|(_, layer)| match &layer.declaration.access.label {
                None => true,
                Some(label) => resolve_label(label)
                    .is_some_and(|term| satisfied.binary_search(&term).is_ok()),
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
        ExistenceCriterion, Hierarchy, HierarchyKind, LayerAccess, MembershipSource,
    };

    fn declaration(name: &str) -> LayerDeclaration {
        LayerDeclaration {
            name: name.into(),
            title: name.into(),
            slices: vec!["default".into()],
            membership: MembershipSource::Enumerated,
            access: LayerAccess {
                label: None,
                artifacts_carry_own: false,
            },
            visible_when: Some(ExistenceCriterion::MinVisible(50)),
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
        d.access.label = Some(label.into());
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

        let resolved = reg.resolve_for(&[TermId::new(7)], |label| match label {
            "clearance:ts" => Some(TermId::new(99)),
            _ => None,
        });

        assert!(resolved.contains("clusters/open"));
        assert!(!resolved.contains("clusters/secret"));
        assert!(!resolved.contains("clusters/never-existed"));
        // The resolved set names only what may be known — a client cannot count what it cannot see.
        assert_eq!(resolved.names().collect::<Vec<_>>(), vec!["clusters/open"]);

        // And with the term: the same layer resolves.
        let cleared = reg.resolve_for(&[TermId::new(7), TermId::new(99)], |label| match label {
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

        let resolved = reg.resolve_for(&[TermId::new(1), TermId::new(2)], |_| None);
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
        let resolved = reg.resolve_for(&[], |_| None);
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
