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
use tessera_types::EntityId;

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
    /// A caller asked for artifacts to be *derived* into a layer whose artifacts are its own to
    /// name — the mirror of [`RegistryError::NotEnumerated`], and a defect rather than caller input.
    NotDerived { layer: String },
    /// An artifact's declared box and its layer's `shape` do not agree — one without the other, or
    /// a stored membership beside a live rule.
    Shape { layer: String, detail: String },
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
    /// An artifact carries an access label on a layer whose `artifact_visibility` names no field,
    /// so nothing would read it.
    Access { layer: String, key: Option<String> },
    /// An artifact's access labels are more, or longer, than a stored record can hold.
    AccessTooLong { layer: String, key: Option<String> },
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
    /// A growth named a key this level does not hold, on a layer whose value set is **closed**.
    ///
    /// Closed is declare-then-use, and the refusal is the whole of it: a mistyped id would
    /// otherwise carry away the members it stole from a real artifact, whose masked count then goes
    /// quietly short. Under `value_set = "open"` the same key mints instead
    /// (`artifacts-from-points.md` §3).
    NoSuchArtifact {
        layer: String,
        level: u32,
        key: String,
    },
    /// An **open** layer met a key it would have minted, and the layer's own declaration says a
    /// minted artifact could not be served: a minted artifact carries nothing but its name, and
    /// this layer requires more of every artifact it publishes.
    ///
    /// **The same two refusals a publication already makes**, hoisted to admission so they refuse
    /// the one batch rather than the window it would have joined — and the same two a build makes
    /// over a member table, which is what keeps one declaration from meaning two things at the two
    /// entry points ([decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
    Unmintable {
        layer: String,
        key: String,
        why: String,
    },
    /// The entity space could not supply the layer's entity or its reserved runs.
    Alloc(AllocError),
    /// A list column's adjacency named one parent for an artifact and the layer holds another.
    ///
    /// Two spellings of one edge, disagreeing: the same refusal a build makes when two points name
    /// different parents for one cluster. There is no correct output — choosing between them would
    /// publish a hierarchy the caller did not write. **A `dag` layer never makes it**: its list
    /// column declares no edges, so no edge reaches here from one.
    ContradictedParent {
        layer: String,
        level: u32,
        child: String,
        claimed: String,
        held: String,
    },
    /// A publication named several parents for one artifact on a layer whose kind gives a child
    /// one — the build's `two_parents` refusal at this entry point, in the same words. A `dag`
    /// layer records them instead (`dag-hierarchies.md` §4, decision 0117).
    SeveralParents {
        layer: String,
        level: u32,
        child: String,
        parents: Vec<String>,
        kind: String,
    },
    /// The edges a publication creates close a cycle — a self-edge being the cycle of length one.
    /// A hierarchy has a root to descend a cut from and a cycle has none, so it is refused at every
    /// kind and at both entry points (`dag-hierarchies.md` §4). The path is child → parent, the
    /// first key repeated at the end.
    Cycle {
        layer: String,
        level: u32,
        cycle: Vec<String>,
    },
    /// A record supplied a fixed part the artifact already holds, and the two differ
    /// (`ingest.md` §1.1, §1.5). A fixed part is written once; the record names the part and
    /// never the held value, which is the caller's own data and may be another caller's.
    PartConflict {
        layer: String,
        level: u32,
        key: String,
        part: String,
    },
    /// A record for a group-scoped layer names no view, or a record for an entity-scoped layer
    /// names one (`ingest.md` §1.5, `views.md` §3.5). The view is part of the identity on a
    /// layer whose artifacts are a set per view, and it is not a part any later record may fill:
    /// an artifact published without it would be drawn on every view of the group, and one
    /// published with it on an entity-scoped layer would name a set that layer does not have.
    ViewIdentity {
        layer: String,
        key: String,
        detail: String,
    },
    /// An edge names an artifact in another view. **Edges may not cross views** (`views.md`
    /// §3.5): a group's views are separate artifact sets, so a parent or an attachment target in
    /// another view is a key that resolves to an artifact this one is not drawn beside.
    CrossViewEdge {
        layer: String,
        level: u32,
        child: String,
        parent: String,
        held_in: Vec<String>,
    },
    /// A record spelled its membership by exclusion on a key the level already holds
    /// (`ingest.md` §1.3, the idempotency column). The membership is the complement taken at the
    /// moment the artifact was published; a second exclusion would mean a *different* set, taken
    /// over the entities that exist now, so it is the `409` a differing fixed part is rather than
    /// a join.
    ExclusionOnHeldKey {
        layer: String,
        level: u32,
        key: String,
    },
    /// One batch names a key more than once and one of its rows carries a fixed part. Two rows
    /// filling one part would each compare it as absent, and the second's record would fail at
    /// apply after the batch was acknowledged; refused before anything is appended, naming the
    /// key. A key repeated with members alone is a join twice and stays lawful.
    RepeatedKey { layer: String, key: String },
    /// A page named entities leaving a **membership** (`ingest.md` §10, R7). A membership never
    /// shrinks: the routes by which a member leaves one are write-path §5.4's two removal rules,
    /// and a second route would re-expose items a suppression or a deletion took out. The one set
    /// a page may shrink is a generating set, named by its rank.
    MembershipShrink { layer: String, key: String },
    /// A page named a rank the artifact holds no content at (`ingest.md` §1.1). Ranks are
    /// positions in the artifact's ranked contents, so a page past the last one names a content
    /// between two that does not exist, or one a withdrawal removed.
    NoSuchContent {
        layer: String,
        key: String,
        rank: u16,
    },
    /// One row carried a set page and a fixed part. A row moves one set or fills fixed parts; a
    /// content supplied beside its own set is a publication, which is the route that takes the two
    /// together (`ingest.md` §1.5).
    SetBesidePart { layer: String, key: String },
}

/// **No artifact is about to exist** — the `pending` answer every caller but the ingest route's
/// mint pass gives [`LayerRegistry::parent_ref`] and [`LayerRegistry::prepare_publish`].
pub fn no_pending(_: &str) -> Option<crate::wal::ParentRef> {
    None
}

/// What [`LayerRegistry::prepare_put`] prepared for one `PUT` batch: the records to append, in
/// the order to apply them, and what the acknowledgement reports.
///
/// The publication first, so that a fill or a growth on a held artifact never precedes the
/// record of a sibling it may name; then the fills; then the growth. One fsync covers them all,
/// as one covers a commit window's records.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedPut {
    /// The new artifacts, or `None` where every key was held.
    pub publish: Option<WalRecord>,
    /// One [`WalRecord::ArtifactFill`] per fixed part filled on a held artifact.
    pub fills: Vec<WalRecord>,
    /// The members joining held artifacts, or `None` where none were.
    pub growth: Option<WalRecord>,
    /// One entity per artifact in the caller's order: a held artifact's own, or the one the
    /// publication claimed.
    pub entities: Vec<tessera_types::EntityId>,
    /// How many artifacts the publication created.
    pub created: u64,
    /// How many created artifacts carry no content on a layer that declares some (R5).
    pub without_content: u64,
    /// How many memberships the batch adds: every member of a created artifact, and every member
    /// a held artifact did not already hold ([`crate::membership::members_added`]).
    pub joined: u64,
}

/// What [`LayerRegistry::prepare_grow`] prepared for one `PATCH` batch.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedGrow {
    /// The set deltas — members joining a membership, and members joining or leaving a generating
    /// set — or `None` where no row moved a set.
    pub growth: Option<WalRecord>,
    /// One [`WalRecord::ArtifactFill`] per fixed part filled.
    pub fills: Vec<WalRecord>,
    /// Per join in the caller's order, how many of its parts were filled.
    pub filled: Vec<u64>,
    /// `(the row's position in the batch, the rank withdrawn)` for every page that emptied a
    /// content's generating set (`ingest.md` §1.1). The content record is removed by the same
    /// delta, and the acknowledgement reports it beside the key so the caller knows to supply the
    /// content again.
    pub withdrawn: Vec<(usize, u16)>,
}

/// One page of a generating set, checked against the set the record holds and turned into the
/// delta that moves it (`ingest.md` §1.1).
///
/// **The cardinality is computed here, from the set the store holds, and travels in the record**:
/// the page says which entities join and leave, and the number the executor publishes beside the
/// operator at the tick is the one this derived. Deriving it at apply instead would let a replay
/// against a differently-repaired set publish a pair that was never derived together.
///
/// `None` where the page changes nothing — an entry already in the state the record asks for is a
/// no-op — and no record is owed for it. **A page that empties the set withdraws the content**,
/// which the delta itself carries: cardinality zero is the withdrawal, and nothing else records it
/// (decision 0107's rule, and 0135's amendment putting the set in the caller's hands).
#[allow(clippy::too_many_arguments)]
fn prepare_set_page(
    layer_name: &str,
    level: u32,
    ordinal: u32,
    rank: u16,
    join: &crate::membership::IncomingGrowth,
    store: &ArtifactStore,
    withdrawn: &mut Vec<(usize, u16)>,
    index: usize,
) -> Result<Option<crate::wal::MembershipGrowth>, RegistryError> {
    let content = store
        .get(layer_name, level, ordinal)
        .and_then(|record| record.contents.get(rank as usize))
        .ok_or_else(|| RegistryError::NoSuchContent {
            layer: layer_name.to_string(),
            key: join.key.clone(),
            rank,
        })?;
    // Joins before leaves, which is the order the store applies them in and the order that makes
    // a paged replacement safe (`ingest.md` §1.1, §2.2).
    let mut next = content.generated_from.clone();
    next.or_inplace(&join.joining);
    next.andnot_inplace(&join.leaving);
    if next == content.generated_from {
        return Ok(None);
    }
    let cardinality = next.cardinality();
    if cardinality == 0 {
        withdrawn.push((index, rank));
    }
    Ok(Some(crate::wal::MembershipGrowth {
        ordinal,
        joining: crate::membership::serialise_members(&join.joining),
        leaving: crate::membership::serialise_members(&join.leaving),
        set: crate::wal::GrownSet::GeneratingSet { rank, cardinality },
    }))
}

/// What [`LayerRegistry::check_edge`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeCheck {
    /// The layer holds exactly this edge.
    Agrees,
    /// The child exists and holds no parent at all, so the edge is the one the artifact is missing.
    /// The caller carries it to the close and records it there through
    /// [`LayerRegistry::prepare_parent_fill`].
    Records,
    /// The child does not exist yet and is one of the keys this batch is about to mint, so the edge
    /// is the minted artifact's own parent rather than a claim about a stored one. It travels on the
    /// publication that creates the child.
    Mints,
}

/// The first cycle in a child → parents adjacency over batch positions, as the path that closes
/// it — `[a, b, …, a]` — or `None` where every walk reaches a root.
///
/// One depth-first pass with three colours, started in position order so the cycle reported is the
/// same one for the same batch: a node met while still on the chain being walked is the cycle, and
/// the chain from its first occurrence is the path. A self-edge is `[a, a]`.
fn first_cycle(adjacency: &[Vec<usize>]) -> Option<Vec<usize>> {
    // 0 unvisited · 1 on the chain being walked · 2 known to reach a root
    let mut state = vec![0u8; adjacency.len()];
    let mut chain: Vec<(usize, usize)> = Vec::new();
    for start in 0..adjacency.len() {
        if state[start] != 0 {
            continue;
        }
        state[start] = 1;
        chain.push((start, 0));
        while let Some((node, next)) = chain.last_mut() {
            let node = *node;
            match adjacency[node].get(*next) {
                None => {
                    state[node] = 2;
                    chain.pop();
                }
                Some(&up) => {
                    *next += 1;
                    match state[up] {
                        2 => {}
                        1 => {
                            let from = chain.iter().position(|(n, _)| *n == up)?;
                            let mut cycle: Vec<usize> =
                                chain[from..].iter().map(|(n, _)| *n).collect();
                            cycle.push(up);
                            return Some(cycle);
                        }
                        _ => {
                            state[up] = 1;
                            chain.push((up, 0));
                        }
                    }
                }
            }
        }
    }
    None
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
            RegistryError::Shape { layer, detail } => {
                write!(f, "layer '{layer}': {detail}")
            }
            RegistryError::NotDerived { layer } => write!(
                f,
                "layer '{layer}' does not take derived artifacts: its membership is a stored set \
                 or a shape, so its artifacts are the caller's to publish rather than identities a \
                 rule produces"
            ),
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
            RegistryError::AccessTooLong { layer, key } => write!(
                f,
                "{layer}: the artifact{} carries more than {} access labels, or one longer than {} \
                 bytes; send fewer or shorter labels",
                match key {
                    Some(key) => format!(" keyed {key}"),
                    None => String::new(),
                },
                u16::MAX - 1,
                u16::MAX - 1
            ),
            RegistryError::Access { layer, key } => write!(
                f,
                "{layer}: the artifact{} carries an access label, and this layer's \
                 `artifact_visibility` names no field, so nothing would read it. Declare \
                 `artifact_visibility.field` on the layer, or send the artifact without `access`",
                match key {
                    Some(key) => format!(" keyed {key}"),
                    None => String::new(),
                }
            ),
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
            RegistryError::NoSuchArtifact { layer, level, key } => write!(
                f,
                "{layer} level {level} holds no artifact under the key {key}, so there is \
                 nothing for these members to join; declare `value_set = \"open\"` on the layer \
                 for a key nothing declares to create the artifact it names"
            ),
            RegistryError::Unmintable { layer, key, why } => write!(
                f,
                "{layer} is declared open, so the key {key} would create the artifact it names — \
                 but {why}. A minted artifact carries nothing but its name"
            ),
            RegistryError::ContradictedParent {
                layer,
                level,
                child,
                claimed,
                held,
            } => write!(
                f,
                "a list column names {claimed} as the parent of {child} in level {level} of \
                 {layer}, which holds {held} as its parent. The two are spellings of one edge, so \
                 publishing either would state a hierarchy nobody wrote"
            ),
            RegistryError::SeveralParents {
                layer,
                level,
                child,
                parents,
                kind,
            } => write!(
                f,
                "{child} in level {level} of {layer} is named as a child of both {}. The layer is \
                 declared {kind}, whose child has one parent — declare it dag if a child may sit \
                 under several",
                parents.join(" and ")
            ),
            RegistryError::Cycle {
                layer,
                level,
                cycle,
            } => write!(
                f,
                "{layer} level {level}: the edges hold a cycle — {}. A hierarchy has a root to \
                 descend a cut from and a cycle has none, so the publication is refused",
                cycle.join(" → ")
            ),
            RegistryError::PartConflict {
                layer,
                level,
                key,
                part,
            } => write!(
                f,
                "{layer} level {level}: the artifact keyed {key} already holds its {part}, and \
                 the record supplies a different one. A fixed part is written once; what changes \
                 an artifact is a delete and a re-publish (ingest.md §1.5)"
            ),
            RegistryError::RepeatedKey { layer, key } => write!(
                f,
                "{layer}: the key {key} appears more than once in this batch and a row of it \
                 carries a fixed part or pages a generating set. A fixed part is filled once, and \
                 a page is read against the set as the batch found it while a withdrawal moves \
                 the ranks above it — so name a key once per batch where it carries a parent, an \
                 attachment, a content, a shape or a rank, and send a second page as a second \
                 request"
            ),
            RegistryError::ViewIdentity {
                layer,
                key,
                detail,
            } => write!(f, "{layer}: the artifact keyed {key}: {detail}"),
            RegistryError::CrossViewEdge {
                layer,
                level,
                child,
                parent,
                held_in,
            } => write!(
                f,
                "{layer} level {level}: the artifact keyed {child} names {parent} as a parent or \
                 an attachment target, and this level holds that key in {} rather than in this \
                 artifact's view. A group's views are separate artifact sets and an edge may not \
                 cross them (views §3.5); publish the parent in this view, or the child in the \
                 parent's",
                held_in.join(", ")
            ),
            RegistryError::ExclusionOnHeldKey { layer, level, key } => write!(
                f,
                "{layer} level {level}: the artifact keyed {key} already exists and this record \
                 spells its membership by exclusion. An exclusion is complemented once, against \
                 the entities that existed at that moment, so a second one would mean a different \
                 set rather than the same one; name the members joining instead (ingest.md §1.3)"
            ),
            RegistryError::MembershipShrink { layer, key } => write!(
                f,
                "{layer}: the page for {key} names members leaving and no rank. A membership \
                 never shrinks (ingest.md §10, R7); name the rank of a content to page its \
                 generating set, or delete the members through /control/changes"
            ),
            RegistryError::NoSuchContent { layer, key, rank } => write!(
                f,
                "{layer}: the artifact keyed {key} holds no content at rank {rank}, so there is \
                 no generating set to page. A content is supplied at publication or filled by \
                 PATCH before its set is paged (ingest.md §1.5)"
            ),
            RegistryError::SetBesidePart { layer, key } => write!(
                f,
                "{layer}: the row for {key} names a rank and carries a fixed part. A row pages one \
                 set or fills fixed parts; supply a content and its set together at publication \
                 (ingest.md §1.5)"
            ),
            RegistryError::Alloc(e) => write!(f, "{e}"),
        }
    }
}

/// The keys a batch names more than once where any of their rows carries a fixed part or pages a
/// generating set — the [`RegistryError::RepeatedKey`] check, one body for both routes. What
/// counts as carrying is the caller's; the growth route's argument is on
/// [`LayerRegistry::prepare_grow`].
fn repeated_key_with_parts<'a>(
    rows: impl Iterator<Item = (Option<&'a str>, Option<&'a str>, bool)>,
) -> Option<String> {
    let mut seen: std::collections::HashMap<(Option<&str>, &str), (usize, bool)> =
        std::collections::HashMap::new();
    for (view, key, carries_part) in rows {
        let Some(key) = key else { continue };
        let entry = seen.entry((view, key)).or_insert((0, false));
        entry.0 += 1;
        entry.1 |= carries_part;
    }
    let mut repeated: Vec<&str> = seen
        .iter()
        .filter(|(_, (count, carries_part))| *count > 1 && *carries_part)
        .map(|((_, key), _)| *key)
        .collect();
    repeated.sort_unstable();
    repeated.first().map(|key| key.to_string())
}

/// A batch's own keys by `(view, key)`, each at the ordinal the publication will give it — what
/// answers a parent naming a sibling this batch is about to create (`ingest.md` §1.5).
///
/// **Owned keys.** The lookup runs behind `&dyn Fn(&str)`, whose argument outlives nothing, so a
/// borrowed key cannot be probed with; a publication is bounded at
/// `max_artifacts_per_request` and the pair is allocated once per named parent.
type SiblingIndex = std::collections::HashMap<(Option<String>, String), u32>;

/// The ordinal `index` gives a key inside one view, as a parent reference at `level`.
fn sibling_ordinal(
    index: &SiblingIndex,
    level: u32,
    view: Option<String>,
) -> impl Fn(&str) -> Option<crate::wal::ParentRef> + '_ {
    move |key: &str| {
        index
            .get(&(view.clone(), key.to_string()))
            .map(|ordinal| crate::wal::ParentRef {
                level,
                ordinal: *ordinal,
            })
    }
}

/// The refusal a parent key that this level holds **in another view** deserves, or `None` where
/// no view holds it and the key is simply missing (`views.md` §3.5).
fn crossing(
    layer_name: &str,
    level: u32,
    view: Option<&str>,
    child_key: Option<&str>,
    parent_key: &str,
    store: &ArtifactStore,
) -> Option<RegistryError> {
    // **Every level up to this one**, because a tiered layer's parent sits at a coarser level:
    // a key held at a coarser level in another view is reported as the crossing it is, which is
    // what the caller has to act on either way — the parent they named is in a set this artifact
    // is not drawn beside.
    let mut held_in: Vec<String> = (0..=level)
        .flat_map(|at| store.views_holding_key(layer_name, at, view, parent_key))
        .collect();
    held_in.sort_unstable();
    held_in.dedup();
    if held_in.is_empty() {
        return None;
    }
    Some(RegistryError::CrossViewEdge {
        layer: layer_name.to_string(),
        level,
        child: child_key.unwrap_or("<no key>").to_string(),
        parent: parent_key.to_string(),
        held_in,
    })
}

/// The view a record belongs to, checked against its layer's scope (`ingest.md` §1.5): required
/// on a group-scoped layer and refused on an entity-scoped one, at both entry points and before
/// anything is allocated.
///
/// **`view` is part of the identity and never a fillable part.** A group-scoped artifact
/// published without one would be indistinguishable from every other view's, and the keys of two
/// views would collide under one index; an entity-scoped artifact published with one would name
/// a set its layer does not have.
fn scoped_view<'a>(
    layer_name: &str,
    scope: &tessera_types::layer::LayerScope,
    key: Option<&str>,
    view: Option<&'a str>,
) -> Result<Option<&'a str>, RegistryError> {
    let named = || key.unwrap_or("<no key>").to_string();
    match (scope.group(), view) {
        (Some(_), Some(view)) => Ok(Some(view)),
        (None, None) => Ok(None),
        (Some(group), None) => Err(RegistryError::ViewIdentity {
            layer: layer_name.to_string(),
            key: named(),
            detail: format!(
                "this layer is scoped to the group '{group}', so its artifacts are a different \
                 set per view and each belongs to one. Name the view on the record: it is part of \
                 the identity, keys being unique per (layer, view), and no later record can fill \
                 it (views §3.5)"
            ),
        }),
        (None, Some(view)) => Err(RegistryError::ViewIdentity {
            layer: layer_name.to_string(),
            key: named(),
            detail: format!(
                "the record names the view '{view}' and this layer is entity-scoped — one \
                 artifact set, drawn on every view it names — so there is no per-view set for it \
                 to belong to. Declare `scope = {{ group = … }}` on the layer, or drop the view \
                 (views §3.5)"
            ),
        }),
    }
}

/// The one rule on what access label an artifact may carry: any a stored record can hold, on a
/// layer whose `artifact_visibility` names a field, and none elsewhere.
fn check_access(
    layer_name: &str,
    declaration: &LayerDeclaration,
    key: Option<&str>,
    access: &[Vec<u8>],
) -> Result<(), RegistryError> {
    if access.is_empty() {
        return Ok(());
    }
    if !declaration.artifact_visibility.carries_own_labels() {
        return Err(RegistryError::Access {
            layer: layer_name.to_string(),
            key: key.map(str::to_string),
        });
    }
    // A packed record counts its labels and their lengths in a `u16`, whose top value marks a
    // record it could not write.
    if access.len() >= u16::MAX as usize || access.iter().any(|d| d.len() >= u16::MAX as usize) {
        return Err(RegistryError::AccessTooLong {
            layer: layer_name.to_string(),
            key: key.map(str::to_string),
        });
    }
    Ok(())
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
            version: self.version + 1,
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
    /// nothing. The control plane's `PUT` partitions a batch before it reaches here
    /// ([`prepare_put`]), so a held key is compared under the fill rule there and this refusal is
    /// the ingest route's mint pass's, where a held key means a resolution went wrong.
    ///
    /// [`prepare_put`]: LayerRegistry::prepare_put
    ///
    /// [`prepare_create`]: LayerRegistry::prepare_create
    /// `pending` is what [`parent_ref`] answers a parent key with when the artifact naming it does
    /// not exist yet and is not in this batch either — empty for every caller but the ingest route's
    /// mint pass, which fixes a tiered chain's ordinals a level at a time and has applied none of
    /// them ([`no_pending`] is the empty answer).
    ///
    /// [`parent_ref`]: LayerRegistry::parent_ref
    pub fn prepare_publish(
        &self,
        layer_name: &str,
        level: u32,
        incoming: &[IncomingArtifact],
        store: &ArtifactStore,
        alloc: &mut Allocator,
        pending: &dyn Fn(&str) -> Option<crate::wal::ParentRef>,
    ) -> Result<WalRecord, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        // **An attribute layer's membership is *evaluated*, never enumerated** — publishing one
        // would install a frozen answer beside a live predicate, and the two would diverge at the
        // first ingest. Refused at the boundary rather than reconciled later.
        //
        // **A spatial layer is not refused here, and the difference is where the shape comes
        // from.** Its artifacts *are* published — a roster of boxes an author wrote — and what is
        // never stored is their membership, which the shape decides at request time. So the
        // refusal below is the same rule in both cases (*a stored answer may not sit beside a live
        // rule*) and only the attribute kind has an answer to store.
        if matches!(layer.declaration.membership, MembershipSource::Attribute(_)) {
            return Err(RegistryError::NotEnumerated {
                layer: layer_name.to_string(),
            });
        }
        self.prepare_artifacts(layer_name, level, incoming, store, alloc, pending)
            .map(|(record, _)| record)
    }

    /// The control plane's publication (`PUT /control/layers/{name}/artifacts`, `ingest.md`
    /// §1.5): [`prepare_publish`] with the batch **partitioned before any ordinal is claimed**
    /// into the keys the level holds and the keys it does not.
    ///
    /// A held key is not refused. Its record is compared part by part with the artifact held
    /// under it, on the fill rule: a fixed part the artifact lacks is filled by its own
    /// [`WalRecord::ArtifactFill`], a part held identically is accepted with no effect, and a part
    /// held differently refuses the whole batch naming the part
    /// ([`RegistryError::PartConflict`]). The membership is a set part and joins by an
    /// [`WalRecord::ArtifactGrow`]; a generating set is compared by bitmap equality, a set changing
    /// only by a page at its rank. A held key resolves to its existing ordinal, so a new artifact naming
    /// it as a parent or as an attachment target lands on that ordinal, and no second artifact is
    /// ever minted under a held key. The new keys go through [`prepare_publish`]'s own body.
    ///
    /// Every comparison and every resolution runs before the first allocation, so a refusal
    /// spends nothing, on [`prepare_publish`]'s contract; the caller appends every record the
    /// answer carries, syncs once, and applies them in the order given.
    ///
    /// [`prepare_publish`]: LayerRegistry::prepare_publish
    pub fn prepare_put(
        &self,
        layer_name: &str,
        level: u32,
        incoming: &[IncomingArtifact],
        store: &ArtifactStore,
        alloc: &mut Allocator,
    ) -> Result<PreparedPut, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        if matches!(layer.declaration.membership, MembershipSource::Attribute(_)) {
            return Err(RegistryError::NotEnumerated {
                layer: layer_name.to_string(),
            });
        }
        if layer.runs.get(level as usize).is_none() {
            return Err(RegistryError::NoSuchLevel {
                layer: layer_name.to_string(),
                level,
            });
        }

        // The partition. A key the level holds is a held artifact; everything else, a key it does
        // not hold or no key at all, is new. Resolved against the store's key index and never the
        // served view, on `resolve_or_mint`'s argument: a suppressed artifact resolves like any
        // other, so its key is never minted again under it.
        // **The view each record names, checked against the layer's scope before anything else**
        // (`ingest.md` §1.5): it is part of the identity, so every lookup below — the held-key
        // partition, the duplicate check, the parent and the attachment — is made inside it.
        let views: Vec<Option<&str>> = incoming
            .iter()
            .map(|artifact| {
                scoped_view(
                    layer_name,
                    &layer.declaration.scope,
                    artifact.key.as_deref(),
                    artifact.view.as_deref(),
                )
            })
            .collect::<Result<_, _>>()?;
        if let Some(key) =
            repeated_key_with_parts(incoming.iter().zip(&views).map(|(artifact, view)| {
                (
                    *view,
                    artifact.key.as_deref(),
                    !artifact.parent_keys.is_empty()
                        || artifact.attached_to.is_some()
                        || !artifact.contents.is_empty()
                        || artifact.shape.is_some(),
                )
            }))
        {
            return Err(RegistryError::RepeatedKey {
                layer: layer_name.to_string(),
                key,
            });
        }
        let held: Vec<Option<u32>> = incoming
            .iter()
            .zip(&views)
            .map(|(artifact, view)| {
                artifact
                    .key
                    .as_deref()
                    .and_then(|key| store.ordinal_of_key(layer_name, level, *view, key))
            })
            .collect();
        let fresh: Vec<IncomingArtifact> = incoming
            .iter()
            .zip(&held)
            .filter(|(_, held)| held.is_none())
            .map(|(artifact, _)| artifact.clone())
            .collect();

        // **The new artifacts' ordinals, fixed before anything is allocated** — the same cursor
        // `prepare_artifacts` reads, so the two agree — and the edges they create, resolved here
        // a second time so the cycle walk below sees them beside the held artifacts' fills.
        // Resolving twice costs a key lookup per parent and keeps the walk ahead of the
        // allocation.
        let first_ordinal = store.next_ordinal(layer_name, level);
        // **Keyed by `(view, key)`, because that is the identity** (`ingest.md` §1.5): a sibling
        // this batch is about to create answers a parent lookup only inside its own view, so an
        // edge cannot be resolved across a group's sets by arriving in one batch.
        let fresh_index: SiblingIndex = fresh
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                a.key
                    .clone()
                    .map(|key| ((a.view.clone(), key), first_ordinal + i as u32))
            })
            .collect();
        let pending_in =
            |view: Option<&str>| sibling_ordinal(&fresh_index, level, view.map(str::to_string));
        let mut batch_edges: BTreeMap<crate::wal::ParentRef, Vec<crate::wal::ParentRef>> =
            BTreeMap::new();
        for (i, artifact) in fresh.iter().enumerate() {
            let child = crate::wal::ParentRef {
                level,
                ordinal: first_ordinal + i as u32,
            };
            let view = artifact.view.as_deref();
            let pending = pending_in(view);
            let parents: Vec<crate::wal::ParentRef> = artifact
                .parent_keys
                .iter()
                .map(|key| {
                    self.parent_ref(
                        layer_name,
                        level,
                        view,
                        artifact.key.as_deref(),
                        key,
                        store,
                        &pending,
                    )
                })
                .collect::<Result<_, _>>()?;
            batch_edges.insert(child, parents);
        }

        // The held artifacts: every part compared, every fill prepared, and the walk run over
        // the layer's edges and the batch's own, before any ordinal is claimed.
        let mut fills = Vec::new();
        // The delta and never the whole list, so a re-`PUT` whose members are all held appends
        // no growth: a set part already in the state the record asks for is a no-op.
        let mut joins: Vec<(u32, croaring::Bitmap)> = Vec::new();
        for ((artifact, ordinal), view) in incoming.iter().zip(&held).zip(&views) {
            let Some(ordinal) = *ordinal else { continue };
            let key = artifact.key.as_deref().expect("a held key is a key");
            // **A second `excluding` on a held key is a `409`** (`ingest.md` §1.3): the
            // complement was taken over the entities that existed at the first publication, so
            // the same list means a different set now, and the fill rule's *present and
            // identical* cannot be asserted of it.
            if artifact.excluding.is_some() {
                return Err(RegistryError::ExclusionOnHeldKey {
                    layer: layer_name.to_string(),
                    level,
                    key: key.to_string(),
                });
            }
            let pending = pending_in(*view);
            let record = store
                .get(layer_name, level, ordinal)
                .expect("the key index names a record");
            let parts = crate::membership::FixedParts {
                parent_keys: artifact.parent_keys.clone(),
                attached_to: artifact.attached_to.clone(),
                contents: artifact
                    .contents
                    .iter()
                    .enumerate()
                    .map(|(rank, content)| (rank as u16, content.values.clone()))
                    .collect(),
                shape: artifact.shape.clone(),
                access: artifact.access.clone(),
            };
            let prepared = self.prepare_fills(
                layer_name,
                level,
                ordinal,
                *view,
                key,
                &parts,
                store,
                &pending,
                &mut batch_edges,
            )?;
            // A generating set is compared by equality, never joined: a set changes by a page at
            // its rank (`ingest.md` §1.1), so a re-`PUT` says the set it said before or refuses. A
            // set beside a content the artifact does not hold is refused for the same reason: a
            // content fill carries no set, and dropping one the caller supplied would serve the
            // content against a set they did not declare.
            for (rank, content) in artifact.contents.iter().enumerate() {
                match record.contents.get(rank) {
                    Some(held) if held.generated_from != content.generated_from => {
                        return Err(RegistryError::PartConflict {
                            layer: layer_name.to_string(),
                            level,
                            key: key.to_string(),
                            part: format!("content[{rank}].generated_from"),
                        });
                    }
                    Some(_) => {}
                    None if !content.generated_from.is_empty() => {
                        return Err(RegistryError::Content {
                            layer: layer_name.to_string(),
                            detail: format!(
                                "the artifact keyed {key}: content[{rank}] is a fill and carries \
                                 a generating set, and a content fill carries none (ingest.md \
                                 §1.5); a content and its set are supplied together on a new \
                                 artifact's publication, and the set moves afterwards by a page \
                                 at its rank"
                            ),
                        });
                    }
                    None => {}
                }
            }
            fills.extend(prepared.into_iter().map(|part| WalRecord::ArtifactFill {
                layer: layer_name.to_string(),
                level,
                ordinal,
                part,
            }));
            let delta = artifact.members.andnot(&record.members);
            if !delta.is_empty() {
                joins.push((ordinal, delta));
            }
        }
        let growth = crate::membership::growth_record(
            layer_name,
            level,
            joins.iter().map(|(ordinal, delta)| (*ordinal, delta)),
        );

        let (publish, without_content) = if fresh.is_empty() {
            (None, 0)
        } else {
            // `no_pending`, and not this batch's own index: [`Self::prepare_artifacts`] builds
            // the same index over the same slice, keyed by `(view, key)`, and answers a sibling
            // from it. The argument is for a caller minting artifacts *elsewhere* — the ingest
            // route's mint pass — which is entity-scoped.
            let (record, without_content) =
                self.prepare_artifacts(layer_name, level, &fresh, store, alloc, &no_pending)?;
            (Some(record), without_content)
        };

        // The entities, in the caller's order: the held artifact's own, or the one the
        // publication claimed, read back off the record.
        let mut claimed = match &publish {
            Some(WalRecord::ArtifactPublish { artifacts, .. }) => {
                artifacts.iter().map(|a| a.entity).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        }
        .into_iter();
        let entities = held
            .iter()
            .map(|held| match held {
                Some(ordinal) => {
                    store
                        .get(layer_name, level, *ordinal)
                        .expect("the key index names a record")
                        .entity
                }
                None => claimed
                    .next()
                    .expect("the publication claimed one entity per new artifact"),
            })
            .collect();

        let joined = publish
            .iter()
            .chain(&growth)
            .map(|record| crate::membership::members_added(record, store))
            .sum();
        Ok(PreparedPut {
            publish,
            fills,
            growth,
            entities,
            created: fresh.len() as u64,
            without_content,
            joined,
        })
    }

    /// The fills a record's fixed parts call for on the artifact at `ordinal`, each compared with
    /// the part held under the fill rule (`ingest.md` §1.1): absent is filled, identical is
    /// nothing, different is [`RegistryError::PartConflict`] naming the part.
    ///
    /// `pending` answers a parent key the batch is about to create; `batch_edges` holds every edge
    /// the batch creates, by child, and gains this artifact's parent list so a later fill in the
    /// same batch walks it. The cycle walk runs over the layer's held edges and the batch's as one
    /// graph (`ingest.md` §1.5, R4): from each named parent upward, refusing if it reaches the
    /// child. Serial on the executor, so no two requests can each pass and together close a
    /// cycle.
    #[allow(clippy::too_many_arguments)]
    fn prepare_fills(
        &self,
        layer_name: &str,
        level: u32,
        ordinal: u32,
        // `view` is the one this artifact belongs to (`ingest.md` §1.5), inside which every key
        // a filled part names is resolved: a lineage or attachment fill may no more cross views
        // than a publication's edge may.
        view: Option<&str>,
        key: &str,
        parts: &crate::membership::FixedParts,
        store: &ArtifactStore,
        pending: &dyn Fn(&str) -> Option<crate::wal::ParentRef>,
        batch_edges: &mut BTreeMap<crate::wal::ParentRef, Vec<crate::wal::ParentRef>>,
    ) -> Result<Vec<crate::wal::ArtifactPart>, RegistryError> {
        use crate::wal::ArtifactPart;
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        let record = store
            .get(layer_name, level, ordinal)
            .expect("a resolved ordinal names a record");
        let conflict = |part: String| RegistryError::PartConflict {
            layer: layer_name.to_string(),
            level,
            key: key.to_string(),
            part,
        };
        let mut fills = Vec::new();

        if !parts.parent_keys.is_empty() {
            let mut resolved: Vec<crate::wal::ParentRef> = parts
                .parent_keys
                .iter()
                .map(|parent| {
                    self.parent_ref(layer_name, level, view, Some(key), parent, store, pending)
                })
                .collect::<Result<_, _>>()?;
            resolved.sort_unstable();
            resolved.dedup();
            let kind = layer.declaration.hierarchy.kind;
            if resolved.len() > 1 && kind != tessera_types::layer::HierarchyKind::Dag {
                let mut named = parts.parent_keys.clone();
                named.sort_unstable();
                named.dedup();
                return Err(RegistryError::SeveralParents {
                    layer: layer_name.to_string(),
                    level,
                    child: key.to_string(),
                    parents: named,
                    kind: format!("{kind:?}").to_lowercase(),
                });
            }
            let child = crate::wal::ParentRef { level, ordinal };
            if record.parents.is_empty() {
                if let Some(cycle) =
                    self.cycle_through(layer_name, store, batch_edges, child, &resolved)
                {
                    return Err(RegistryError::Cycle {
                        layer: layer_name.to_string(),
                        level,
                        cycle: cycle
                            .into_iter()
                            .map(|at| self.key_at(layer_name, at, store))
                            .collect(),
                    });
                }
                batch_edges.insert(child, resolved.clone());
                fills.push(ArtifactPart::Parents(resolved));
            } else if record.parents != resolved {
                return Err(conflict("parent".to_string()));
            }
        }

        if let Some(wanted) = &parts.attached_to {
            let attachment = self.resolve_attachment(layer_name, view, key, wanted, store)?;
            match &record.attached_to {
                None => fills.push(ArtifactPart::AttachedTo(crate::wal::PublishedAttachment {
                    layer: attachment.layer,
                    level: attachment.level,
                    ordinal: attachment.ordinal,
                    entity: attachment.entity,
                })),
                Some(held) if *held == attachment => {}
                Some(_) => return Err(conflict("attached_to".to_string())),
            }
        }

        if let Some(shape) = &parts.shape {
            if layer.declaration.shape.is_none() {
                return Err(RegistryError::Shape {
                    layer: layer_name.to_string(),
                    detail: format!(
                        "the artifact keyed {key} is given a shape, and this layer declares no \
                         `shape`. Its members come from the stored set its membership names, so \
                         a shape beside them is a region nothing evaluates"
                    ),
                });
            }
            match store.shape_of(layer_name, level, ordinal) {
                None => fills.push(ArtifactPart::Shape(shape.clone())),
                Some(held) if held.digest() == shape.digest() => {}
                Some(_) => return Err(conflict("shape".to_string())),
            }
        }

        if !parts.access.is_empty() {
            check_access(layer_name, &layer.declaration, Some(key), &parts.access)?;
            let access = crate::membership::canonical_access(&parts.access);
            if record.access.is_empty() {
                fills.push(ArtifactPart::Access(access));
            } else if record.access != access {
                return Err(conflict("access".to_string()));
            }
        }

        let declared = &layer.declaration.content.supplied;
        let refuse_content = |detail: String| RegistryError::Content {
            layer: layer_name.to_string(),
            detail: format!("the artifact keyed {key}: {detail}"),
        };
        // Ranks fill in order within one record, so a record carrying two new ranks sees the
        // first before it checks the second.
        let mut next_rank = record.contents.len();
        let mut supplied: Vec<&(u16, Vec<String>)> = parts.contents.iter().collect();
        supplied.sort_by_key(|(rank, _)| *rank);
        for (rank, values) in supplied {
            let rank = *rank as usize;
            if declared.is_empty() {
                return Err(refuse_content(
                    "carries supplied content, and this layer declares none — the kinds a client \
                     may draw come from the layer's declaration, so content under no declared \
                     kind could never be served"
                        .to_string(),
                ));
            }
            if values.len() != declared.len() {
                return Err(refuse_content(format!(
                    "content[{rank}] supplies {} value(s) for {} declared kind(s); every entry is \
                     a whole description, and a viewer is served one of them entire or no \
                     artifact at all",
                    values.len(),
                    declared.len()
                )));
            }
            let digest = crate::membership::content_digest(values);
            if let Some(held) = record.contents.get(rank) {
                if held.digest != digest {
                    return Err(conflict(format!("content[{rank}]")));
                }
                continue;
            }
            if rank != next_rank {
                return Err(refuse_content(format!(
                    "content[{rank}] names a rank past the next one, {next_rank}; ranks are \
                     positions in the artifact's list of contents and fill in order"
                )));
            }
            // A fill carries values and no generating set (T2b's page brings the set), so on a
            // layer that tests one it would be content served to everyone: the refusal a
            // publication with an empty set draws, at the same door.
            if declared
                .iter()
                .any(|s| s.require_member_visibility.requires_all_members())
            {
                return Err(refuse_content(format!(
                    "content[{rank}] is a fill, and a content fill carries no generating set \
                     (ingest.md §1.5); this layer's content requires every member visible, so a \
                     content and its set are supplied together on a new artifact's publication, \
                     and the set moves afterwards by a page at its rank"
                )));
            }
            if store.content_row_is_packed(layer_name, level, ordinal) {
                return Err(refuse_content(format!(
                    "content[{rank}] would be a further content on an artifact whose content row \
                     is in a durable extent; the record blob holds one row per artifact until it \
                     reads per column (ingest.md §1.4, T3), so the rank cannot be written"
                )));
            }
            fills.push(ArtifactPart::Content {
                rank: rank as u16,
                values: values.clone(),
                digest,
            });
            next_rank += 1;
        }

        Ok(fills)
    }

    /// Resolve an attachment a record names, on the publication's rules: the target layer is one
    /// this layer declares in `depends_on`, and the target exists.
    fn resolve_attachment(
        &self,
        layer_name: &str,
        view: Option<&str>,
        key: &str,
        wanted: &crate::membership::IncomingAttachment,
        store: &ArtifactStore,
    ) -> Result<crate::membership::Attachment, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
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
        // **An attachment stays inside its view where the target layer has one set per view**
        // (`views.md` §3.5): the target is looked up under this artifact's own view, and a key
        // this level holds in another view is named as the crossing it is rather than reported
        // as a missing target.
        let in_view = target.declaration.scope.group().and(view);
        let ordinal = match store.ordinal_of_key(&wanted.layer, wanted.level, in_view, &wanted.key)
        {
            Some(ordinal) => ordinal,
            None => {
                let held_in =
                    store.views_holding_key(&wanted.layer, wanted.level, in_view, &wanted.key);
                if !held_in.is_empty() {
                    return Err(RegistryError::CrossViewEdge {
                        layer: layer_name.to_string(),
                        level: wanted.level,
                        child: key.to_string(),
                        parent: wanted.key.clone(),
                        held_in,
                    });
                }
                return Err(missing());
            }
        };
        let entity = target
            .runs
            .get(wanted.level as usize)
            .and_then(|runs| runs.entity_of(ordinal as u64))
            .ok_or_else(missing)?;
        Ok(crate::membership::Attachment {
            layer: wanted.layer.clone(),
            level: wanted.level,
            ordinal,
            entity: EntityId::new(entity),
        })
    }

    /// The cycle a parent list on `child` would close, walking **up** from each named parent
    /// through the layer's held edges and the batch's own as one graph (`ingest.md` §1.5, R4):
    /// the path `[child, parent, …, child]` by position, or `None` where every walk reaches a
    /// root. Over artifacts and never members, so rung 3's 30,954 nodes and 42,287 edges are
    /// microseconds (modelled).
    fn cycle_through(
        &self,
        layer_name: &str,
        store: &ArtifactStore,
        batch_edges: &BTreeMap<crate::wal::ParentRef, Vec<crate::wal::ParentRef>>,
        child: crate::wal::ParentRef,
        parents: &[crate::wal::ParentRef],
    ) -> Option<Vec<crate::wal::ParentRef>> {
        let parents_of = |at: crate::wal::ParentRef| -> Vec<crate::wal::ParentRef> {
            let mut up: Vec<crate::wal::ParentRef> =
                batch_edges.get(&at).cloned().unwrap_or_default();
            if let Some(record) = store.get(layer_name, at.level, at.ordinal) {
                up.extend(record.parents.iter().copied());
            }
            up
        };
        let mut visited: BTreeSet<crate::wal::ParentRef> = BTreeSet::new();
        for &start in parents {
            // Depth first with the chain kept, so the refusal can say the path.
            let mut chain: Vec<(crate::wal::ParentRef, Vec<crate::wal::ParentRef>)> =
                vec![(start, parents_of(start))];
            if start == child {
                return Some(vec![child, child]);
            }
            visited.insert(start);
            while let Some((_, up)) = chain.last_mut() {
                let Some(next) = up.pop() else {
                    chain.pop();
                    continue;
                };
                if next == child {
                    let mut cycle = vec![child];
                    cycle.extend(chain.iter().map(|(at, _)| *at));
                    cycle.push(child);
                    return Some(cycle);
                }
                if visited.insert(next) {
                    chain.push((next, parents_of(next)));
                }
            }
        }
        None
    }

    /// **Mint the artifacts a predicate's own rule names** — the values an attribute column
    /// carries, one artifact per distinct value, keyed by the value's spelling.
    ///
    /// **The rule is the membership, so this is the only route into such a layer.**
    /// [`prepare_publish`] refuses an attribute layer because a caller's stored answer would sit
    /// beside a live predicate and diverge from it at the first ingest; what arrives here is not an
    /// answer but the *identities* the rule produces, which have to exist somewhere for a
    /// suppression to land on and for an edge to name. Each carries its key and nothing else: no
    /// membership (the column is the membership), no content, no attachment and no parent, each of
    /// which `LayerDeclaration::validate` already refuses such a layer from declaring.
    ///
    /// **Suppression-blindness carries over unchanged**, because the duplicate-key check
    /// [`prepare_artifacts`] makes reads [`ArtifactStore::ordinal_of_key`] — the store's key index,
    /// which loses a key at exactly one event, the fold retiring the artifact's own entity. A
    /// suppressed value's key therefore still resolves, is refused as a duplicate, and never mints a
    /// second unsuppressed artifact (§5's third ruling, stated in full on [`resolve_or_mint`]). A
    /// *deleted* value's key does mint again, and the new artifact is a new object with a new
    /// entity — which is what a deletion means.
    ///
    /// [`prepare_publish`]: LayerRegistry::prepare_publish
    /// [`prepare_artifacts`]: LayerRegistry::prepare_artifacts
    /// [`resolve_or_mint`]: LayerRegistry::resolve_or_mint
    pub fn prepare_derive(
        &self,
        layer_name: &str,
        level: u32,
        keys: &[String],
        store: &ArtifactStore,
        alloc: &mut Allocator,
    ) -> Result<WalRecord, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        // The mirror of [`prepare_publish`]'s refusal, and it exists for the same reason read the
        // other way: an enumerated layer's artifacts are the caller's to name, so minting one from
        // a rule would be the service inventing an identity nobody published.
        if !matches!(layer.declaration.membership, MembershipSource::Attribute(_)) {
            return Err(RegistryError::NotDerived {
                layer: layer_name.to_string(),
            });
        }
        let incoming: Vec<IncomingArtifact> = keys
            .iter()
            .map(|key| IncomingArtifact {
                key: Some(key.clone()),
                // A predicate layer's artifacts are derived from a value column, which is
                // entity-scoped; a group-scoped one is refused at `scoped_view`.
                view: None,
                members: croaring::Bitmap::new(),
                excluding: None,
                contents: Vec::new(),
                attached_to: None,
                parent_keys: Vec::new(),
                shape: None,
                access: Vec::new(),
            })
            .collect();
        self.prepare_artifacts(
            layer_name,
            level,
            &incoming,
            store,
            alloc,
            &crate::no_pending,
        )
        .map(|(record, _)| record)
    }

    /// The allocation and validation both entry points share — everything [`prepare_publish`] does
    /// once the membership source has been checked.
    ///
    /// **One body, so a rule cannot hold at one entry point and not the other.** The two callers
    /// differ in exactly which memberships they admit; the duplicate-key rule, the content rules,
    /// the attachment resolution, the parent resolution and the reservation growth are one
    /// implementation.
    ///
    /// [`prepare_publish`]: LayerRegistry::prepare_publish
    fn prepare_artifacts(
        &self,
        layer_name: &str,
        level: u32,
        incoming: &[IncomingArtifact],
        store: &ArtifactStore,
        alloc: &mut Allocator,
        pending: &dyn Fn(&str) -> Option<crate::wal::ParentRef>,
    ) -> Result<(WalRecord, u64), RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        let runs = layer
            .runs
            .get(level as usize)
            .ok_or_else(|| RegistryError::NoSuchLevel {
                layer: layer_name.to_string(),
                level,
            })?;

        // **The view each record names, checked against the layer's scope** (`ingest.md` §1.5),
        // before a key is compared or an ordinal claimed: it is the scope every key below is
        // unique within.
        let views: Vec<Option<&str>> = incoming
            .iter()
            .map(|artifact| {
                scoped_view(
                    layer_name,
                    &layer.declaration.scope,
                    artifact.key.as_deref(),
                    artifact.view.as_deref(),
                )
            })
            .collect::<Result<_, _>>()?;

        // Duplicate keys, against the level and against the rest of the batch. Both, because a
        // batch that repeats a key internally would otherwise publish two artifacts under one name
        // and leave the index pointing at whichever landed last. **Per `(view, key)`**: on a
        // group-scoped layer the same key in two views is two artifacts, which is what `view`
        // being part of the identity means.
        let mut within_batch = BTreeSet::new();
        for (artifact, view) in incoming.iter().zip(&views) {
            let Some(key) = &artifact.key else {
                continue;
            };
            if store
                .ordinal_of_key(layer_name, level, *view, key)
                .is_some()
                || !within_batch.insert((*view, key))
            {
                return Err(RegistryError::DuplicateKey {
                    layer: layer_name.to_string(),
                    key: key.clone(),
                });
            }
        }

        // **What the layer declares is what every content must carry**, checked once here rather
        // than discovered per request. The refusals are one rule read three ways: a client draws
        // what `/v1/meta` says the layer carries, so a served content must never lack a declared
        // kind, must never carry an undeclared one, and must never carry a generating set nothing
        // will test.
        //
        // **An artifact with no content on a layer that declares some is accepted and counted**
        // (`ingest.md` §1.5, R5). The serving path withholds it until a content is filled
        // (decision 0076: served whole or not at all), which a viewer cannot tell from content
        // withheld, so it discloses nothing; the count is the caller's fidelity signal.
        let declared = &layer.declaration.content.supplied;
        let requires_all_members = declared
            .iter()
            .any(|s| s.require_member_visibility.requires_all_members());
        let without_content = incoming
            .iter()
            .filter(|artifact| !declared.is_empty() && artifact.contents.is_empty())
            .count() as u64;
        for artifact in incoming {
            check_access(
                layer_name,
                &layer.declaration,
                artifact.key.as_deref(),
                &artifact.access,
            )?;
        }
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

        // **A declared shape and a declared membership are the same statement**, so an artifact
        // must carry exactly the one its layer names. Both halves are refusals rather than
        // tolerated absences: an artifact of a shape layer with no box has no membership rule at
        // all — it counts zero for every viewer and is absent under any criterion, which no client
        // can tell from an artifact whose members are simply invisible to them — and a box on a
        // layer that declares no shape is a region nothing evaluates, which would read on
        // `/v1/meta` as geometry the service holds and does not.
        let declares_shape = layer.declaration.shape.is_some();
        for (i, artifact) in incoming.iter().enumerate() {
            match (declares_shape, &artifact.shape) {
                (true, None) => {
                    return Err(RegistryError::Shape {
                        layer: layer_name.to_string(),
                        detail: format!(
                            "artifact {i} of this batch carries no shape, and this layer's \
                             `shape` declares one. The shape is the whole of such an artifact's \
                             membership, so one published without it would count zero for every \
                             viewer"
                        ),
                    })
                }
                (false, Some(_)) => {
                    return Err(RegistryError::Shape {
                        layer: layer_name.to_string(),
                        detail: format!(
                            "artifact {i} of this batch carries a shape, and this layer declares \
                             no `shape`. Its members come from the stored set its membership \
                             names, so a shape beside them is a region nothing evaluates"
                        ),
                    })
                }
                _ => {}
            }
            // A shape layer stores no membership: the tiles covering the box decide who belongs, at
            // request time. A set beside it would be a frozen answer next to a live rule — the same
            // thing `prepare_publish` refuses an attribute layer for.
            if declares_shape && !artifact.members.is_empty() {
                return Err(RegistryError::Shape {
                    layer: layer_name.to_string(),
                    detail: format!(
                        "artifact {i} of this batch carries a stored membership, and this layer's \
                         members come from its shape at request time — a set stored beside a live \
                         rule is a frozen answer that diverges from it at the first ingest"
                    ),
                });
            }
        }

        // **Attachments, resolved before anything is allocated.** The caller names a target by the
        // key they published it under — an ordinal is never disclosed (C8), so a key is the
        // only address they hold — and what is stored is the resolved `(level, ordinal, entity)`.
        // Resolving once here rather than per request is what makes the extra predicate term one
        // `verdict` lookup instead of a registry walk.
        let attachments: Vec<Option<crate::membership::Attachment>> = incoming
            .iter()
            .zip(&views)
            .map(|(artifact, view)| {
                let Some(wanted) = &artifact.attached_to else {
                    // **A layer that declares a dependency publishes only dependents** (decision
                    // 0089). Refused here rather than served ungated: the serving predicate reads
                    // the prerequisite off the attachment, so an artifact carrying none would be
                    // the one artifact of a label layer that answered on its own conjuncts alone.
                    if !layer.declaration.depends_on.is_empty() {
                        return Err(RegistryError::MissingAttachment {
                            layer: layer_name.to_string(),
                            key: artifact
                                .key
                                .clone()
                                .unwrap_or_else(|| "<no key>".to_string()),
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
                // The target's own view, on `resolve_attachment`'s rule: inside this artifact's
                // view where the target layer is group-scoped, and a key held only in another
                // view is the crossing rather than a missing target (`views.md` §3.5).
                let in_view = target.declaration.scope.group().and(*view);
                let ordinal =
                    match store.ordinal_of_key(&wanted.layer, wanted.level, in_view, &wanted.key) {
                        Some(ordinal) => ordinal,
                        None => {
                            let held_in = store.views_holding_key(
                                &wanted.layer,
                                wanted.level,
                                in_view,
                                &wanted.key,
                            );
                            if !held_in.is_empty() {
                                return Err(RegistryError::CrossViewEdge {
                                    layer: layer_name.to_string(),
                                    level: wanted.level,
                                    child: artifact
                                        .key
                                        .clone()
                                        .unwrap_or_else(|| "<no key>".to_string()),
                                    parent: wanted.key.clone(),
                                    held_in,
                                });
                            }
                            return Err(missing());
                        }
                    };
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

        // The parent each artifact names, resolved by the layer's declared edge shape — see
        // [`LayerRegistry::parent_ref`], which the ingest route's edge check shares. A child's
        // parent is often a **sibling in this batch** that has no ordinal until this call assigns
        // one, which is what `batch_ordinal` answers and why the resolution takes it.
        // **Indexed once rather than scanned per lookup.** `parent_ref` asks this before it asks
        // the store, so a level whose artifacts name siblings pays a linear pass over the batch for
        // every one of them — O(n²), and reachable at the sizes this stage publishes, which is the
        // very thing [`ArtifactStore::keys`] exists to avoid for the duplicate check above. The
        // first position is the *only* position: that duplicate check has already refused a batch
        // repeating a key, so the map answers exactly what `position` did.
        let batch_index: SiblingIndex = incoming
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                a.key
                    .clone()
                    .map(|key| ((a.view.clone(), key), first_ordinal as u32 + i as u32))
            })
            .collect();
        // **A sibling answers inside the naming artifact's own view** (`ingest.md` §1.5), so a
        // batch carrying one key in two views resolves each child's parent to the one beside it.
        let batch_ordinal = |view: Option<&str>| {
            let sibling = sibling_ordinal(&batch_index, level, view.map(str::to_string));
            move |key: &str| sibling(key).or_else(|| pending(key))
        };
        // **Several parents are a `dag` layer's to hold and every other kind's to refuse**
        // (`dag-hierarchies.md` §4, decision 0117). Ascending and deduplicated, so a key named
        // twice is one edge and the record's list is the order every reader assumes.
        let kind = layer.declaration.hierarchy.kind;
        let name_of = |i: usize| {
            incoming[i]
                .key
                .clone()
                .unwrap_or_else(|| format!("the artifact at ordinal {}", first_ordinal + i as u64))
        };
        let parents: Vec<Vec<crate::wal::ParentRef>> = incoming
            .iter()
            .zip(&views)
            .enumerate()
            .map(|(i, (artifact, view))| {
                let pending = batch_ordinal(*view);
                let mut resolved: Vec<crate::wal::ParentRef> = artifact
                    .parent_keys
                    .iter()
                    .map(|key| {
                        self.parent_ref(
                            layer_name,
                            level,
                            *view,
                            artifact.key.as_deref(),
                            key,
                            store,
                            &pending,
                        )
                    })
                    .collect::<Result<_, _>>()?;
                resolved.sort_unstable();
                resolved.dedup();
                if resolved.len() > 1 && kind != tessera_types::layer::HierarchyKind::Dag {
                    let mut named = artifact.parent_keys.clone();
                    named.sort_unstable();
                    named.dedup();
                    return Err(RegistryError::SeveralParents {
                        layer: layer_name.to_string(),
                        level,
                        child: name_of(i),
                        parents: named,
                        kind: format!("{kind:?}").to_lowercase(),
                    });
                }
                Ok(resolved)
            })
            .collect::<Result<_, _>>()?;

        // **The cycle check, over the edges this publication creates** (`dag-hierarchies.md` §4).
        // A held artifact's edges were settled before this batch and none of them can name an
        // artifact that does not exist yet, so an edge from a new artifact into a held one cannot
        // close a cycle on its own: every cycle among these records is among the artifacts this
        // publication mints, and the batch's own adjacency is what has to be walked. The one way
        // a held artifact gains an edge is a fill of its parent list, and `prepare_put` walks the
        // layer and the batch as one graph for that before it reaches here. The ingest route's
        // `mint_records` reaches this through `prepare_publish`, so the check holds at both entry
        // points from one body. A tiered layer's parents sit at coarser levels and never in the
        // batch, so its adjacency here is empty, which is the shape that cannot hold a cycle.
        let batch_end = first_ordinal as u32 + incoming.len() as u32;
        let adjacency: Vec<Vec<usize>> = parents
            .iter()
            .map(|resolved| {
                resolved
                    .iter()
                    .filter(|p| {
                        p.level == level
                            && p.ordinal >= first_ordinal as u32
                            && p.ordinal < batch_end
                    })
                    .map(|p| (p.ordinal - first_ordinal as u32) as usize)
                    .collect()
            })
            .collect();
        if let Some(cycle) = first_cycle(&adjacency) {
            return Err(RegistryError::Cycle {
                layer: layer_name.to_string(),
                level,
                cycle: cycle.into_iter().map(name_of).collect(),
            });
        }

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
                    // **Part of the identity on a group-scoped layer** (`ingest.md` §1.5),
                    // checked against the layer's scope above and recorded here, so replay lands
                    // the artifact in the view it was acked in.
                    view: artifact.view.clone(),
                    members: serialise_members(&artifact.members),
                    contents: artifact
                        .contents
                        .iter()
                        .map(|v| crate::wal::PublishedContent {
                            values: v.values.clone(),
                            digest: crate::membership::content_digest(&v.values),
                            generated_from: serialise_members(&v.generated_from),
                            cardinality: v.generated_from.cardinality(),
                        })
                        .collect(),
                    attached_to: attachments[i]
                        .as_ref()
                        .map(|a| crate::wal::PublishedAttachment {
                            layer: a.layer.clone(),
                            level: a.level,
                            ordinal: a.ordinal,
                            entity: a.entity,
                        }),
                    parents: parents[i].clone(),
                    shape: artifact.shape.clone(),
                    access: crate::membership::canonical_access(&artifact.access),
                }
            })
            .collect();

        Ok((
            WalRecord::ArtifactPublish {
                layer: layer_name.to_string(),
                level,
                extend_runs,
                artifacts,
            },
            without_content,
        ))
    }

    /// Validates a batch of joins against the artifacts they name and returns the record that makes
    /// them durable — on [`prepare_publish`]'s contract: the caller appends, syncs, and only then
    /// applies.
    ///
    /// **It allocates nothing, and that is the difference from a publication.** A join takes no
    /// ordinal and no entity: the artifact exists, so the identities it is addressed by exist too.
    /// So there is no allocator here and a refusal spends nothing — where a refused publication has
    /// to be argued about, this one is simply a refusal.
    ///
    /// **A key the level does not hold is refused**, whatever the layer's value set says: this is
    /// a growth naming an artifact to add members to, not a point declaring the artifact it belongs
    /// to, so there is nothing here for an unknown key to have created — see
    /// [`crate::IncomingGrowth`], and [`LayerRegistry::resolve_or_mint`] for the route that does
    /// create one. The resolution is [`ArtifactStore::ordinal_of_key`] — the *store's* index, never
    /// the served view — which is what keeps a suppressed artifact from reading as absent and
    /// having a second, unsuppressed artifact minted under its key (§5).
    ///
    /// **Nothing joining is not an error.** A caller may honestly name an artifact and add nothing
    /// to it; `Ok(None)` says the record would be empty and no append is owed. Refusing would block
    /// a write over an input that discloses nothing.
    ///
    /// [`prepare_publish`]: LayerRegistry::prepare_publish
    pub fn prepare_grow(
        &self,
        layer_name: &str,
        level: u32,
        incoming: &[crate::membership::IncomingGrowth],
        store: &ArtifactStore,
    ) -> Result<PreparedGrow, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;

        // A predicate layer's membership is evaluated per request, so there is nothing to grow —
        // and a stored answer beside a live predicate is exactly what `prepare_publish` refuses for
        // the same reason.
        if layer.declaration.membership != MembershipSource::Enumerated {
            return Err(RegistryError::NotEnumerated {
                layer: layer_name.to_string(),
            });
        }
        if layer.runs.get(level as usize).is_none() {
            return Err(RegistryError::NoSuchLevel {
                layer: layer_name.to_string(),
                level,
            });
        }

        // Every key resolves and every part compares before anything is written: the whole batch
        // or none of it, on `prepare_publish`'s rule. A partially applied growth would leave a
        // caller unable to say which of their joins happened.
        let mut growth = Vec::with_capacity(incoming.len());
        let mut fills = Vec::new();
        let mut filled = Vec::with_capacity(incoming.len());
        // **A row naming a rank counts as carrying a part here**, so a key repeated with ranks
        // refuses the batch and a key repeated with members alone stays lawful (a join twice).
        //
        // A page is checked against the set the store holds and the store applies the pages of one
        // record in order, so two rows against one artifact would each be prepared against the
        // state before the batch. That is not merely a stale count: a page that empties a set
        // withdraws its content and moves every rank above it down, so a second row's rank then
        // names a different content than the caller wrote — and where the two sets happen to be
        // the same size, the cardinality check that would otherwise catch it agrees by
        // coincidence, leaving one content serving against its old set and another holding a
        // member nobody declared for it. Modelling the shift instead would have to decide what
        // the caller's second rank meant after the first row moved it, which is a question the
        // design does not answer; a caller who wants two pages sends two requests, and each one's
        // ranks are read against the state the previous acknowledgement reported.
        //
        // Per `(view, key)`: on a group-scoped layer one key in two views is two artifacts.
        if let Some(key) = repeated_key_with_parts(incoming.iter().map(|join| {
            (
                join.view.as_deref(),
                Some(join.key.as_str()),
                !join.parts.is_empty() || join.rank.is_some(),
            )
        })) {
            return Err(RegistryError::RepeatedKey {
                layer: layer_name.to_string(),
                key,
            });
        }
        let mut batch_edges = BTreeMap::new();
        let mut withdrawn = Vec::new();
        for (index, join) in incoming.iter().enumerate() {
            let ordinal =
                self.resolve_growth_key(layer_name, level, join.view.as_deref(), &join.key, store)?;
            if let Some(rank) = join.rank {
                if !join.parts.is_empty() {
                    return Err(RegistryError::SetBesidePart {
                        layer: layer_name.to_string(),
                        key: join.key.clone(),
                    });
                }
                filled.push(0);
                if let Some(delta) = prepare_set_page(
                    layer_name,
                    level,
                    ordinal,
                    rank,
                    join,
                    store,
                    &mut withdrawn,
                    index,
                )? {
                    growth.push(delta);
                }
                continue;
            }
            // **A membership never shrinks** (`ingest.md` §10, R7), refused before any part is
            // compared so that a caller who meant a generating set is told which spelling they
            // wanted rather than having their joins applied and their leaves dropped.
            if !join.leaving.is_empty() {
                return Err(RegistryError::MembershipShrink {
                    layer: layer_name.to_string(),
                    key: join.key.clone(),
                });
            }
            let parts = self.prepare_fills(
                layer_name,
                level,
                ordinal,
                join.view.as_deref(),
                &join.key,
                &join.parts,
                store,
                &no_pending,
                &mut batch_edges,
            )?;
            filled.push(parts.len() as u64);
            fills.extend(parts.into_iter().map(|part| WalRecord::ArtifactFill {
                layer: layer_name.to_string(),
                level,
                ordinal,
                part,
            }));
            if join.joining.is_empty() {
                continue;
            }
            // **What the artifact already holds is not a join.** A page restating a membership the
            // store carries would otherwise append a delta that changes nothing and pin the log at
            // it — the log is reclaimed up to the oldest record a generation still needs, and a
            // growth pin is released by the compaction fold alone. Taken against the membership as
            // it stands before the batch, which is the state every row of the batch is prepared
            // against and the state `growth_receipt` counts `joined` over.
            let mut joining = join.joining.clone();
            if let Some(record) = store.get(layer_name, level, ordinal) {
                joining.andnot_inplace(&record.members);
            }
            if joining.is_empty() {
                continue;
            }
            growth.push(crate::wal::MembershipGrowth {
                ordinal,
                joining: crate::membership::serialise_members(&joining),
                leaving: Vec::new(),
                set: crate::wal::GrownSet::Membership,
            });
        }
        Ok(PreparedGrow {
            growth: (!growth.is_empty()).then(|| WalRecord::ArtifactGrow {
                layer: layer_name.to_string(),
                level,
                growth,
            }),
            fills,
            filled,
            withdrawn,
        })
    }

    /// The ordinal a member key names, or **`None` where the layer is open and nothing holds it**
    /// — the resolution the ingest route makes at admission (`artifacts-from-points.md` §6.3).
    ///
    /// `None` is *this key will be minted at the close*, and it is returned only after the two
    /// checks a minted artifact could not pass are made: a layer declaring supplied content kinds,
    /// and a layer declaring a dependency, each refuse here rather than at the close, so one
    /// caller's key refuses one batch instead of the window it would have joined.
    ///
    /// **Suppression is invisible to this by construction, which is §5's third ruling.** The lookup
    /// is [`ArtifactStore::ordinal_of_key`] — the store's key index — and that index loses a key at
    /// exactly one event, the fold retiring the artifact's own entity, which is a *deletion*. A
    /// suppression touches no stored structure at all (write-path §5.4, Rule S), so there is
    /// nothing here that could see one. Written against the *served* view instead, a suppressed
    /// artifact would read as absent, its key would mint a second artifact, and the new one would
    /// not be suppressed — a suppression defeated by ingesting a point. That is why this reads the
    /// store, and why it must never be replaced by a `verdict` call.
    pub fn resolve_or_mint(
        &self,
        layer_name: &str,
        level: u32,
        view: Option<&str>,
        key: &str,
        store: &ArtifactStore,
    ) -> Result<Option<u32>, RegistryError> {
        match self.resolve_growth_key(layer_name, level, view, key, store) {
            Ok(ordinal) => Ok(Some(ordinal)),
            Err(RegistryError::NoSuchArtifact { layer, level, key }) => {
                let declaration = &self
                    .layers
                    .get(&layer)
                    .expect("resolve_growth_key found the layer before it reached the key")
                    .declaration;
                if declaration.value_set != tessera_types::layer::ValueSet::Open {
                    return Err(RegistryError::NoSuchArtifact { layer, level, key });
                }
                let unmintable = |why: &str| {
                    Err(RegistryError::Unmintable {
                        layer: layer.clone(),
                        key: key.clone(),
                        why: why.to_string(),
                    })
                };
                // The two refusals `prepare_publish` would make of an artifact carrying only a key,
                // made here where the batch can still be rejected without effect. Both are
                // declarations about *every* artifact of the layer, so neither depends on which key
                // arrived — a layer is mintable or it is not.
                if !declaration.content.supplied.is_empty() {
                    return unmintable(&format!(
                        "the layer declares {} supplied content kind(s), and an artifact served \
                         without content its layer declares cannot be told apart from one whose \
                         content was withheld",
                        declaration.content.supplied.len()
                    ));
                }
                if !declaration.depends_on.is_empty() {
                    return unmintable(
                        "the layer declares depends_on, so every artifact it publishes attaches to \
                         one, and an artifact with no dependency would be gated on nothing",
                    );
                }
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// The ordinal a growth's key names, with the layer and level checked — **the resolution half
    /// of [`prepare_grow`], shared with the ingest route through [`resolve_or_mint`].**
    ///
    /// A batch arriving at `/control/ingest` with a column named for a layer resolves its keys here
    /// at admission, before the entities exist, and carries the ordinals to its window's close
    /// (`artifacts-from-points.md` §6.2). The two callers must agree about what a key means and
    /// about what an unknown one costs — which is why it is this function and not a second lookup.
    ///
    /// **The lookup is `ArtifactStore::ordinal_of_key`, never the served view**, and that is what
    /// makes it suppression-blind by construction: a suppressed artifact resolves like any other,
    /// grows like any other, and stays suppressed (§5's third ruling — stated in full on
    /// [`resolve_or_mint`], where the alternative reading would mint).
    ///
    /// [`resolve_or_mint`]: LayerRegistry::resolve_or_mint
    ///
    /// `view` is the key of the view the artifact is in, required on a group-scoped layer and
    /// refused on an entity-scoped one. The growth route names none.
    pub fn resolve_growth_key(
        &self,
        layer_name: &str,
        level: u32,
        view: Option<&str>,
        key: &str,
        store: &ArtifactStore,
    ) -> Result<u32, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        if layer.declaration.membership != MembershipSource::Enumerated {
            return Err(RegistryError::NotEnumerated {
                layer: layer_name.to_string(),
            });
        }
        if layer.runs.get(level as usize).is_none() {
            return Err(RegistryError::NoSuchLevel {
                layer: layer_name.to_string(),
                level,
            });
        }
        let view = scoped_view(layer_name, &layer.declaration.scope, Some(key), view)?;
        store
            .ordinal_of_key(layer_name, level, view, key)
            .ok_or_else(|| RegistryError::NoSuchArtifact {
                layer: layer_name.to_string(),
                level,
                key: key.to_string(),
            })
    }

    /// What one edge a caller's list column declared is against the edge this layer holds: the same
    /// one, one to record, or a contradiction.
    ///
    /// The edge is decided here, at admission, so a contradiction refuses the one batch that carries
    /// it rather than the window it would have joined. Recording happens at the close, where the
    /// ordinals are claimed — [`LayerRegistry::prepare_parent_fill`].
    ///
    /// A parent other than the one the child holds is the build's `two_parents` refusal at this entry
    /// point: there is no correct output, and picking one would publish a hierarchy nobody wrote.
    ///
    /// The two `minting` arguments answer *is this key one the batch is about to create?* — the keys
    /// [`resolve_or_mint`] returned `None` for. They decide two of the three answers:
    ///
    /// - **the child is minting** → [`EdgeCheck::Mints`]: the edge is the new artifact's own
    ///   parent, carried on the publication that creates it.
    /// - **the parent is minting** and the child exists → the layer holds no such parent yet, so a
    ///   child holding no parent is [`EdgeCheck::Records`] and takes the ordinal the close assigns;
    ///   a child holding a different one is the contradiction, because a parent that does not exist
    ///   cannot be the parent it already has.
    ///
    /// **Every edge reaching here is a tree's.** Only a `nested` or `tiered` list column declares
    /// edges; a `dag` layer's list is memberships alone and its several parents arrive on the
    /// artifact row by the publish route (`dag-hierarchies.md` §4, decision 0125), so there is no
    /// kind on which a held parent other than the claimed one is anything but the contradiction.
    ///
    /// **The child's is a fact and the parent's is a search**, which is why one is a `bool` and the
    /// other a closure. A child's level is the edge's own; a parent's is whatever the layer's shape
    /// says to look at — the child's level for a nested layer and any coarser one for a tiered
    /// layer — so the question asked of a parent is *does this layer hold that key anywhere*. Asking
    /// it of the child too would treat a key minting at one level as minting at every level, and a
    /// levelled taxonomy legitimately carries one key at two.
    ///
    /// [`resolve_or_mint`]: LayerRegistry::resolve_or_mint
    pub fn check_edge(
        &self,
        edge: &crate::command::BatchEdge,
        store: &ArtifactStore,
        child_mints: bool,
        parent_mints: &dyn Fn(&str) -> bool,
    ) -> Result<EdgeCheck, RegistryError> {
        let crate::command::BatchEdge {
            layer,
            level,
            view,
            child,
            parent,
        } = edge;
        let (layer, level, view) = (layer.as_str(), *level, view.as_deref());
        if child_mints {
            return Ok(EdgeCheck::Mints);
        }
        if !self.layers.contains_key(layer) {
            return Err(RegistryError::NoSuchLayer(layer.to_string()));
        }
        let ordinal = self.resolve_growth_key(layer, level, view, child, store)?;
        let held: Vec<crate::wal::ParentRef> = store
            .get(layer, level, ordinal)
            .map(|r| r.parents.clone())
            .unwrap_or_default();
        // A tree's child holds at most one parent, so the one it holds is the one named. Only a
        // `nested` or `tiered` list column declares an edge, so every edge reaching here is a
        // tree's: a `dag` layer's parents arrive on the artifact row by the publish route and its
        // list column is memberships alone (decision 0125).
        let contradicted = |held: crate::wal::ParentRef| RegistryError::ContradictedParent {
            layer: layer.to_string(),
            level,
            child: child.clone(),
            claimed: parent.clone(),
            held: self.key_at(layer, held, store),
        };
        if parent_mints(parent) {
            return match held.first() {
                None => Ok(EdgeCheck::Records),
                Some(held) => Err(contradicted(*held)),
            };
        }
        let claimed =
            self.parent_ref(layer, level, view, Some(child), parent, store, &no_pending)?;
        if held.contains(&claimed) {
            return Ok(EdgeCheck::Agrees);
        }
        match held.first() {
            None => Ok(EdgeCheck::Records),
            Some(held) => Err(contradicted(*held)),
        }
    }

    /// The record that gives an existing artifact the parent a list column named, or `None` where it
    /// already holds it.
    ///
    /// A parent list is a fixed part, so this is [`Self::prepare_fills`]'s rule reached from the
    /// ingest door: absent is filled, identical is nothing, a different parent is a refusal, and the
    /// cycle walk runs over the layer's held edges and `window_edges` — every edge this close is
    /// recording — as one graph. `pending` answers a parent key the same close is minting.
    ///
    /// Called at the close and not at admission, because a parent minted here has no ordinal until
    /// the publication that claims it.
    pub fn prepare_parent_fill(
        &self,
        edge: &crate::command::BatchEdge,
        store: &ArtifactStore,
        pending: &dyn Fn(&str) -> Option<crate::wal::ParentRef>,
        window_edges: &mut BTreeMap<crate::wal::ParentRef, Vec<crate::wal::ParentRef>>,
    ) -> Result<Option<WalRecord>, RegistryError> {
        let (layer, level, view) = (edge.layer.as_str(), edge.level, edge.view.as_deref());
        let ordinal = self.resolve_growth_key(layer, level, view, &edge.child, store)?;
        let parts = crate::membership::FixedParts {
            parent_keys: vec![edge.parent.clone()],
            ..Default::default()
        };
        let part = self
            .prepare_fills(
                layer,
                level,
                ordinal,
                view,
                &edge.child,
                &parts,
                store,
                pending,
                window_edges,
            )?
            .pop();
        Ok(part.map(|part| WalRecord::ArtifactFill {
            layer: layer.to_string(),
            level,
            ordinal,
            part,
        }))
    }

    /// The caller's own name for the artifact at a resolved position, for a refusal that has to
    /// mention it. An artifact published without a key has none, and its address is what the caller
    /// can act on instead.
    fn key_at(&self, layer_name: &str, at: crate::wal::ParentRef, store: &ArtifactStore) -> String {
        store
            .get(layer_name, at.level, at.ordinal)
            .and_then(|r| r.key.clone())
            .unwrap_or_else(|| format!("the artifact at level {} ordinal {}", at.level, at.ordinal))
    }

    /// Where a parent key sits, for an artifact at `level` — **one resolution, used by the
    /// publication that stores an edge and by the ingest route that checks one.**
    ///
    /// **Which direction an edge may run is the layer's declaration.** A nested layer's edges relate
    /// artifacts of one level, and a level is normally published in one batch — so a child's parent
    /// is usually a sibling with no ordinal until the publication assigns one, which is what
    /// `pending` answers. A tiered layer's edges run the other way, from a **coarser level** to
    /// this one: its parent was published in an earlier batch, so the store usually answers, and the
    /// search runs over the levels above this one. A key found in two of them is a refusal rather
    /// than a first match, because which one an edge meant would then depend on the search order.
    ///
    /// **`pending` answers for artifacts that are about to exist but do not yet**, at whatever level
    /// they will land: the siblings of a publication's own batch, and the artifacts an ingest batch
    /// is minting a level at a time (`artifacts-from-points.md` §6.3). It returns a whole
    /// [`ParentRef`] rather than an ordinal because a tiered chain's parent sits at a *coarser*
    /// level than the child, and a caller minting level *k* has already fixed the ordinals of level
    /// *k−1* without having applied them anywhere a lookup could see.
    ///
    /// **A layer may not mix the two**, which is what makes the question answerable at all: the
    /// declared kind says which shape its edges have, and an edge of the other shape refuses.
    ///
    /// [`ParentRef`]: crate::wal::ParentRef
    #[allow(clippy::too_many_arguments)]
    pub fn parent_ref(
        &self,
        layer_name: &str,
        level: u32,
        // `view` is the child's, on a group-scoped layer, and the parent key is resolved inside
        // it: **an edge may not cross views** (`views.md` §3.5), so a key this layer holds only
        // in another view is refused as [`RegistryError::CrossViewEdge`] rather than resolved.
        // `None` on an entity-scoped layer, whose keys sit in one set.
        view: Option<&str>,
        child_key: Option<&str>,
        parent_key: &str,
        store: &ArtifactStore,
        pending: &dyn Fn(&str) -> Option<crate::wal::ParentRef>,
    ) -> Result<crate::wal::ParentRef, RegistryError> {
        let layer = self
            .layers
            .get(layer_name)
            .ok_or_else(|| RegistryError::NoSuchLayer(layer_name.to_string()))?;
        let missing = || RegistryError::NoSuchParent {
            layer: layer_name.to_string(),
            level,
            key: parent_key.to_string(),
        };
        let cross_level = matches!(
            layer.declaration.hierarchy.kind,
            tessera_types::layer::HierarchyKind::Tiered
        );
        let edges_allowed = cross_level
            || matches!(
                layer.declaration.hierarchy.kind,
                tessera_types::layer::HierarchyKind::Nested
                    | tessera_types::layer::HierarchyKind::Dag
            );
        if !edges_allowed {
            return Err(RegistryError::EdgesOnUntreedLayer {
                layer: layer_name.to_string(),
                kind: format!("{:?}", layer.declaration.hierarchy.kind).to_lowercase(),
            });
        }
        // **Only a within-level edge can name itself, and one that does is the cycle of length
        // one** (`dag-hierarchies.md` §4). A key is unique per `(layer, level)`, so a levelled
        // taxonomy legitimately carries the same key at two levels — an arXiv archive with no
        // subclass is `hep-ph` at both, and the level-1 artifact's parent is the level-0 one of
        // the same name. Refusing that would force a caller to rename half their taxonomy to
        // satisfy a check meant for a tree.
        if !cross_level && child_key == Some(parent_key) {
            return Err(RegistryError::Cycle {
                layer: layer_name.to_string(),
                level,
                cycle: vec![parent_key.to_string(), parent_key.to_string()],
            });
        }
        let ambiguous = || RegistryError::AmbiguousParent {
            layer: layer_name.to_string(),
            key: parent_key.to_string(),
        };
        if cross_level {
            let mut found = None;
            // **A pending parent is a candidate beside the stored ones, not ahead of them**, so a
            // key held at one coarser level and minted at another is the same ambiguity it would be
            // if both were stored — which is the case the search order must not be allowed to
            // decide.
            for candidate in pending(parent_key)
                .into_iter()
                .filter(|p| p.level < level)
                .chain((0..level).filter_map(|coarser| {
                    store
                        .ordinal_of_key(layer_name, coarser, view, parent_key)
                        .map(|ordinal| crate::wal::ParentRef {
                            level: coarser,
                            ordinal,
                        })
                }))
            {
                if found.is_some_and(|held| held != candidate) {
                    return Err(ambiguous());
                }
                found = Some(candidate);
            }
            // A key that exists only at this level or a finer one is an edge running the wrong way
            // — refused rather than reinterpreted, since a tiered layer's whole guarantee is that
            // lineage never runs against the levels.
            return found.ok_or_else(|| {
                crossing(layer_name, level, view, child_key, parent_key, store)
                    .unwrap_or_else(missing)
            });
        }
        pending(parent_key)
            .filter(|p| p.level == level)
            .or_else(|| {
                store
                    .ordinal_of_key(layer_name, level, view, parent_key)
                    .map(|ordinal| crate::wal::ParentRef { level, ordinal })
            })
            .ok_or_else(|| {
                crossing(layer_name, level, view, child_key, parent_key, store)
                    .unwrap_or_else(missing)
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
            version: self.version + 1,
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
    /// `version` is the counter the manifest saved. The counter resumes from it or from the highest
    /// seeded layer's version, whichever is higher, so a registration replayed or made after the
    /// seed is never given a version lower than one already served.
    pub fn seed(&mut self, layers: &[RegisteredLayer], tombstones: &[String], version: u64) {
        self.version = self.version.max(version);
        for layer in layers {
            self.version = self.version.max(layer.version);
            self.layers
                .insert(layer.declaration.name.clone(), layer.clone());
        }
        self.tombstones.extend(tombstones.iter().cloned());
    }

    /// Record one level's serving layout — the fold's re-evaluation, and the only thing that ever
    /// changes it after a registration
    /// ([decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)).
    ///
    /// **Not a WAL record and not a version bump.** A layout is a latency choice that puts nothing
    /// on the wire: both forms answer identically, so bumping [`RegisteredLayer::version`] would
    /// make every session re-resolve a layer for a change none of them can observe, and a WAL
    /// record would make a replay able to change one. Its durable home is the manifest the same
    /// fold writes — which is why this must be called **before** [`LayerRegistry::snapshot`], and
    /// why a replay that re-applies a `LayerCreate` over a seeded registry returns the level to its
    /// declared pin or to artifact-major. That costs the fold's column its adoption and nothing
    /// else: both routes answer identically, and the membership extents a row form is built from
    /// are written whatever the layout.
    ///
    /// Returns whether the record **moved**, which is what tells the caller a flip happened: a
    /// flipped level's cached forms in the old layout are never asked for again, so something has
    /// to drop them explicitly (selection memo §5).
    pub fn set_layout(
        &mut self,
        layer: &str,
        level: u32,
        layout: tessera_types::layer::ServingLayout,
    ) -> bool {
        let Some(registered) = self.layers.get_mut(layer) else {
            return false;
        };
        // Dense over the levels the layer declares — a record shorter than `runs` reads as
        // artifact-major for the levels past its end, and this is where it stops being short.
        if registered.layouts.len() <= level as usize {
            registered
                .layouts
                .resize(level as usize + 1, Default::default());
        }
        let moved = registered.layouts[level as usize] != layout;
        registered.layouts[level as usize] = layout;
        moved
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
                version,
            } => {
                // The record's own version, so a replay over a seed that already counts it moves
                // nothing.
                let version = *version;
                self.version = self.version.max(version);
                self.layers.insert(
                    declaration.name.clone(),
                    RegisteredLayer {
                        // **The registration's own record of the serving layout**: the declared pin
                        // where there is one, artifact-major where there is not. A level with no
                        // artifacts has no shape to observe — blocks per artifact and the artifact
                        // count are both properties of where the data landed — so the automatic
                        // pick has nothing to read here and takes the conservative answer, which is
                        // the form every derived structure already exists for (decision 0094).
                        //
                        // **A fold re-evaluates it and writes the result into the manifest.** A
                        // replay that re-applies this record over a seeded registry therefore
                        // returns the level to artifact-major, which costs the fold's column its
                        // adoption and nothing else: the membership extents are what a row form is
                        // built from, they are written whatever the layout, and the two routes
                        // answer identically.
                        layouts: RegisteredLayer::initial_layouts(declaration),
                        declaration: (**declaration).clone(),
                        entity: *layer_entity,
                        runs: runs.clone(),
                        version,
                    },
                );
            }
            WalRecord::LayerDrop { name, version } => {
                self.layers.remove(name);
                self.tombstones.insert(name.clone());
                self.version = self.version.max(*version);
            }
            // A publication's only effect on the *registry* is the reservation it grew. The
            // artifacts themselves belong to the store, applied from the same record.
            //
            // **The version does not move.** It keys a session's cached reachability, and
            // publishing artifacts changes who may reach the layer not at all — bumping it would
            // invalidate every open session's resolution on every batch, which at a clustering's
            // publication rate is a re-resolve per request.
            WalRecord::ArtifactPublish {
                layer,
                level,
                extend_runs,
                ..
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
    pub fn resolve_for(&self, admits: impl Fn(&str) -> bool) -> ResolvedLayers {
        let names = self
            .layers
            .iter()
            .filter(|(_, layer)| match &layer.declaration.visibility {
                None => true,
                Some(label) => admits(label),
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
            scope: Default::default(),
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
            layout: None,
            shape: None,
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
        assert_eq!(
            layer.runs.len(),
            1,
            "a level-less layer still holds level 0"
        );
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
        register(
            &mut reg,
            &mut alloc,
            gated("clusters/secret", "clearance:ts"),
        )
        .unwrap();
        register(&mut reg, &mut alloc, declaration("clusters/open")).unwrap();

        let resolved = reg.resolve_for(|_| false);

        assert!(resolved.contains("clusters/open"));
        assert!(!resolved.contains("clusters/secret"));
        assert!(!resolved.contains("clusters/never-existed"));
        // The resolved set names only what may be known — a client cannot count what it cannot see.
        assert_eq!(resolved.names().collect::<Vec<_>>(), vec!["clusters/open"]);

        // And with the term: the same layer resolves.
        let cleared = reg.resolve_for(|label| label == "clearance:ts");
        assert!(cleared.contains("clusters/secret"));
    }

    #[test]
    fn a_resolution_does_not_outlive_the_registry_version_it_was_computed_from() {
        // A gate edit that narrows a layer must not leave open sessions on the pre-edit gate for
        // the rest of their life, which is what a resolution with no version key would do.
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let resolved = reg.resolve_for(|_| false);
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
        let create = live
            .prepare_create(declaration("clusters/a"), &mut alloc)
            .unwrap();
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
            view: None,
            members: croaring::Bitmap::of(members),
            excluding: None,
            contents: Vec::new(),
            attached_to: None,
            parent_keys: Vec::new(),
            shape: None,
            access: Vec::new(),
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
        let record = reg.prepare_publish(layer, 0, incoming, store, alloc, &no_pending)?;
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

    fn treed(name: &str, kind: HierarchyKind) -> LayerDeclaration {
        let mut d = declaration(name);
        d.hierarchy.kind = kind;
        d
    }

    fn under(key: &str, parents: &[&str]) -> IncomingArtifact {
        let mut artifact = incoming(key, &[1, 2]);
        artifact.parent_keys = parents.iter().map(|p| p.to_string()).collect();
        artifact
    }

    /// **A `dag` layer records several parents, ascending and deduplicated; a `nested` layer
    /// refuses them** (`dag-hierarchies.md` §4, decision 0117). The refusal is the build's
    /// `two_parents` at this entry point, and it names both parents.
    #[test]
    fn a_dag_child_holds_every_parent_it_named_and_a_tree_refuses_a_second() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, treed("mesh/d", HierarchyKind::Dag)).unwrap();
        register(
            &mut reg,
            &mut alloc,
            treed("clusters/t", HierarchyKind::Nested),
        )
        .unwrap();

        // Parents named out of order, one of them twice: the record is ascending and holds each
        // once, whatever the caller's spelling.
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "mesh/d",
            &[
                under("p1", &[]),
                under("p0", &[]),
                under("c", &["p1", "p0", "p1"]),
            ],
        )
        .unwrap();
        let child = store.get("mesh/d", 0, 2).unwrap();
        assert_eq!(child.key.as_deref(), Some("c"));
        assert_eq!(
            child.parents,
            vec![
                crate::wal::ParentRef {
                    level: 0,
                    ordinal: 0
                },
                crate::wal::ParentRef {
                    level: 0,
                    ordinal: 1
                }
            ]
        );

        let refused = publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/t",
            &[
                under("p0", &[]),
                under("p1", &[]),
                under("c", &["p1", "p0"]),
            ],
        )
        .expect_err("a tree's child has one parent");
        assert_eq!(
            refused,
            RegistryError::SeveralParents {
                layer: "clusters/t".into(),
                level: 0,
                child: "c".into(),
                parents: vec!["p0".into(), "p1".into()],
                kind: "nested".into(),
            }
        );
        // The same key twice is one edge, and a tree takes it.
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/t",
            &[under("p0", &[]), under("c", &["p0", "p0"])],
        )
        .unwrap();
        assert_eq!(store.get("clusters/t", 0, 1).unwrap().parents.len(), 1);
    }

    /// **A publication whose edges close a cycle is refused, naming the cycle**
    /// (`dag-hierarchies.md` §4) — at every kind, a self-edge being the cycle of length one. A
    /// growth never adds lineage, so the batch's own edges are the whole of what can close one.
    #[test]
    fn a_cycle_among_a_publications_own_edges_is_refused_naming_it() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, treed("mesh/d", HierarchyKind::Dag)).unwrap();
        register(
            &mut reg,
            &mut alloc,
            treed("clusters/t", HierarchyKind::Nested),
        )
        .unwrap();

        let three = [under("a", &["c"]), under("b", &["a"]), under("c", &["b"])];
        for layer in ["mesh/d", "clusters/t"] {
            let refused = publish(&mut reg, &mut store, &mut alloc, layer, &three)
                .expect_err("a 3-cycle has no root");
            assert_eq!(
                refused,
                RegistryError::Cycle {
                    layer: layer.into(),
                    level: 0,
                    cycle: vec!["a".into(), "c".into(), "b".into(), "a".into()],
                },
                "the path is child → parent from the first key in batch order"
            );
            let refused = publish(
                &mut reg,
                &mut store,
                &mut alloc,
                layer,
                &[under("s", &["s"])],
            )
            .expect_err("a self-edge is a cycle of length one");
            assert_eq!(
                refused,
                RegistryError::Cycle {
                    layer: layer.into(),
                    level: 0,
                    cycle: vec!["s".into(), "s".into()],
                }
            );
            assert_eq!(
                store.next_ordinal(layer, 0),
                0,
                "a refusal publishes nothing"
            );
        }

        // A diamond is not a cycle: two paths to one root, every edge descending.
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "mesh/d",
            &[
                under("r", &[]),
                under("l", &["r"]),
                under("m", &["r"]),
                under("c", &["l", "m"]),
            ],
        )
        .expect("a diamond is the shape a dag layer exists for");
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
        assert_eq!(store.ordinal_of_key("clusters/a", 0, None, "c2"), Some(2));

        // Every ordinal resolves back to the entity the level's run gives it, and the layer's own
        // entity is none of them — suppressing the layer must not suppress an artifact.
        let layer = reg.get("clusters/a").unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for ordinal in 0..3u32 {
            let record = store.get("clusters/a", 0, ordinal).unwrap();
            assert_eq!(
                layer.runs[0].entity_of(ordinal as u64),
                Some(record.entity.raw())
            );
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

    /// **The two routes into a layer are exclusive, and which one a layer takes is its membership.**
    ///
    /// An attribute layer's members are evaluated, so an enumerated set beside them is a frozen
    /// answer that diverges from the predicate at the first ingest — publication is refused and the
    /// values are *derived* instead. An enumerated layer is the mirror: its artifacts are the
    /// caller's to name, so deriving one would be the service inventing an identity nobody
    /// published.
    #[test]
    fn a_predicate_layer_is_derived_into_and_never_published_into() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        let mut predicate = declaration("regions/uk");
        predicate.membership = MembershipSource::Attribute("severity".into());
        predicate.require_member_visibility = Some(ExistenceCriterion::Count(25));
        register(&mut reg, &mut alloc, predicate).unwrap();

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

        // The route that *is* open: the values, minted with their keys and nothing else.
        let record = reg
            .prepare_derive(
                "regions/uk",
                0,
                &["high".to_string(), "low".to_string()],
                &store,
                &mut alloc,
            )
            .expect("a predicate layer's values are derived into it");
        reg.apply(&record);
        assert_eq!(store.apply(&record, 0), 0);
        let WalRecord::ArtifactPublish { artifacts, .. } = &record else {
            unreachable!()
        };
        assert_eq!(artifacts.len(), 2);
        for artifact in artifacts {
            assert!(artifact.contents.is_empty(), "a derived artifact has none");
            assert!(artifact.attached_to.is_none());
            assert!(artifact.parents.is_empty());
            assert!(artifact.shape.is_none());
        }
        assert_eq!(store.ordinal_of_key("regions/uk", 0, None, "high"), Some(0));
        assert_eq!(store.ordinal_of_key("regions/uk", 0, None, "low"), Some(1));

        // **A key the level already holds never mints a second artifact**, which is what keeps a
        // suppressed value from being re-minted unsuppressed: the check reads the store's key
        // index, and a suppression touches no stored structure at all.
        assert_eq!(
            reg.prepare_derive("regions/uk", 0, &["high".to_string()], &store, &mut alloc),
            Err(RegistryError::DuplicateKey {
                layer: "regions/uk".into(),
                key: "high".into(),
            })
        );

        // And a level the layer never declared is a refusal too, not an implicit creation.
        assert_eq!(
            reg.prepare_publish("clusters/nope", 0, &[], &store, &mut alloc, &no_pending),
            Err(RegistryError::NoSuchLayer("clusters/nope".into()))
        );
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        assert_eq!(
            reg.prepare_publish("clusters/a", 3, &[], &store, &mut alloc, &no_pending),
            Err(RegistryError::NoSuchLevel {
                layer: "clusters/a".into(),
                level: 3
            })
        );
        // The mirror refusal: an enumerated layer's artifacts are the caller's to name.
        assert_eq!(
            reg.prepare_derive("clusters/a", 0, &["v".to_string()], &store, &mut alloc),
            Err(RegistryError::NotDerived {
                layer: "clusters/a".into()
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
                &no_pending,
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
        assert_eq!(
            replayed_store.ordinal_of_key("clusters/a", 0, None, "c1"),
            Some(1)
        );
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
            .prepare_publish("clusters/a", 0, &full, &store, &mut alloc, &no_pending)
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
        assert_eq!(
            alloc.low_water(),
            after_first - tessera_types::layer::RESERVED_BLOCK
        );
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

    // ---- The fill rule (`ingest.md` §1.1, §1.5; decision 0136 R3, R4, R5) ----------------------

    /// A layer declaring one supplied content that needs no generating set, so a content can be
    /// filled on its own.
    fn described(name: &str) -> LayerDeclaration {
        let mut d = declaration(name);
        d.content.supplied = vec![tessera_types::layer::SuppliedContent {
            name: "topic".into(),
            ty: "text".into(),
            require_member_visibility: tessera_types::layer::SuppliedRequirement::Inherited,
        }];
        d
    }

    /// Apply every record a prepared `PUT` carries, in its order, the way the executor does it.
    fn apply_put(reg: &mut LayerRegistry, store: &mut ArtifactStore, prepared: &PreparedPut) {
        for record in prepared
            .publish
            .iter()
            .chain(prepared.fills.iter())
            .chain(prepared.growth.iter())
        {
            reg.apply(record);
            assert_eq!(store.apply(record, 0), 0);
        }
    }

    fn part_of(record: &WalRecord) -> &crate::wal::ArtifactPart {
        let WalRecord::ArtifactFill { part, .. } = record else {
            panic!("a fill record");
        };
        part
    }

    /// **A `PUT` naming a held key fills what the artifact lacks, accepts what it holds
    /// identically, and refuses what it holds differently, naming the part and never the value.**
    #[test]
    fn a_put_on_a_held_key_fills_absent_parts_and_refuses_a_differing_one_by_name() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        let mut layer = described("topics/t");
        layer.hierarchy.kind = HierarchyKind::Nested;
        register(&mut reg, &mut alloc, layer).unwrap();

        // Published bare: no parent, no content — accepted and counted (R5).
        let first = reg
            .prepare_put(
                "topics/t",
                0,
                &[incoming("root", &[1, 2, 3]), incoming("child", &[1])],
                &store,
                &mut alloc,
            )
            .unwrap();
        assert_eq!((first.created, first.without_content), (2, 2));
        apply_put(&mut reg, &mut store, &first);
        assert_eq!(store.next_ordinal("topics/t", 0), 2);

        // The same keys again, carrying the parts: two fills and a join, nothing minted.
        let mut child = incoming("child", &[1, 4]);
        child.parent_keys = vec!["root".into()];
        child.contents = vec![crate::membership::IncomingContent::new(
            vec!["a topic".into()],
            [],
        )];
        let mark = alloc.low_water();
        let second = reg
            .prepare_put("topics/t", 0, &[child.clone()], &store, &mut alloc)
            .unwrap();
        assert_eq!(alloc.low_water(), mark, "a held key allocates nothing");
        assert!(second.publish.is_none());
        assert_eq!(second.created, 0);
        assert_eq!(
            second.joined, 1,
            "the member the artifact did not hold joins"
        );
        assert_eq!(
            second.fills.len(),
            2,
            "the parent and the content are filled"
        );
        assert_eq!(
            second.entities,
            vec![store.get("topics/t", 0, 1).unwrap().entity],
            "the answer is the held artifact's own entity"
        );
        apply_put(&mut reg, &mut store, &second);
        let held = store.get("topics/t", 0, 1).unwrap();
        assert_eq!(
            held.parents,
            vec![crate::wal::ParentRef {
                level: 0,
                ordinal: 0
            }]
        );
        assert_eq!(held.contents.len(), 1);
        assert_eq!(
            held.contents[0].values.as_deref(),
            Some(&["a topic".to_string()][..])
        );
        assert_eq!(held.members, croaring::Bitmap::of(&[1, 4]));
        assert_eq!(store.next_ordinal("topics/t", 0), 2, "nothing was minted");

        // Identical again: no record at all.
        let third = reg
            .prepare_put("topics/t", 0, &[child], &store, &mut alloc)
            .unwrap();
        assert!(third.publish.is_none() && third.fills.is_empty() && third.growth.is_none());
        assert_eq!((third.created, third.joined), (0, 0));

        // A differing content: refused naming the part, and the held value is not in the text.
        let mut differing = incoming("child", &[]);
        differing.contents = vec![crate::membership::IncomingContent::new(
            vec!["another topic".into()],
            [],
        )];
        let refused = reg
            .prepare_put("topics/t", 0, &[differing], &store, &mut alloc)
            .unwrap_err();
        assert_eq!(
            refused,
            RegistryError::PartConflict {
                layer: "topics/t".into(),
                level: 0,
                key: "child".into(),
                part: "content[0]".into(),
            }
        );
        assert!(
            !refused.to_string().contains("a topic"),
            "the held value is never echoed: {refused}"
        );

        // A differing parent, the same way.
        let other = incoming("other", &[7]);
        let put = reg
            .prepare_put("topics/t", 0, &[other], &store, &mut alloc)
            .unwrap();
        apply_put(&mut reg, &mut store, &put);
        let mut reparented = incoming("child", &[]);
        reparented.parent_keys = vec!["other".into()];
        let refused = reg
            .prepare_put("topics/t", 0, &[reparented], &store, &mut alloc)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::PartConflict { part, .. } if part == "parent"),
            "{refused:?}"
        );
        assert!(!refused.to_string().contains("root"), "{refused}");
    }

    /// **A batch mixing held keys with new ones mints only the new ones, and a new artifact
    /// naming a held sibling as its parent lands on the held ordinal** (R3): no second artifact is
    /// ever minted under a held key.
    #[test]
    fn a_mixed_put_mints_only_the_new_keys_and_resolves_a_held_sibling_parent() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(
            &mut reg,
            &mut alloc,
            treed("clusters/t", HierarchyKind::Nested),
        )
        .unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/t",
            &[under("root", &[])],
        )
        .unwrap();

        let prepared = reg
            .prepare_put(
                "clusters/t",
                0,
                &[
                    under("leaf-a", &["root"]),
                    under("root", &[]),
                    under("leaf-b", &["leaf-a"]),
                ],
                &store,
                &mut alloc,
            )
            .unwrap();
        assert_eq!(prepared.created, 2);
        let Some(WalRecord::ArtifactPublish { artifacts, .. }) = &prepared.publish else {
            panic!("two new artifacts are published");
        };
        assert_eq!(
            artifacts.iter().map(|a| a.ordinal).collect::<Vec<_>>(),
            vec![1, 2],
            "the new ordinals follow the held one"
        );
        assert_eq!(
            artifacts[0].parents,
            vec![crate::wal::ParentRef {
                level: 0,
                ordinal: 0
            }],
            "the held sibling resolves to its existing ordinal"
        );
        assert_eq!(
            artifacts[1].parents,
            vec![crate::wal::ParentRef {
                level: 0,
                ordinal: 1
            }],
            "and a new sibling to the ordinal this batch claims"
        );
        assert_eq!(
            prepared.entities[1],
            store.get("clusters/t", 0, 0).unwrap().entity,
            "the held key answers with the artifact it names, in the caller's order"
        );
        apply_put(&mut reg, &mut store, &prepared);
        assert_eq!(store.next_ordinal("clusters/t", 0), 3);
        assert_eq!(store.ordinal_of_key("clusters/t", 0, None, "root"), Some(0));
    }

    /// **A lineage fill that would close a cycle across held edges and the batch's own is
    /// refused** (R4), with nothing applied and nothing allocated.
    #[test]
    fn a_lineage_fill_closing_a_cycle_across_held_and_batch_edges_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(
            &mut reg,
            &mut alloc,
            treed("clusters/t", HierarchyKind::Nested),
        )
        .unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/t",
            &[under("a", &[]), under("b", &[]), under("c", &["b"])],
        )
        .unwrap();

        // Held edge c → b. A fill b → a is fine; a fill a → c then closes a → c → b → a.
        let fill = |reg: &LayerRegistry, store: &ArtifactStore, child: &str, parent: &str| {
            let mut join = crate::membership::IncomingGrowth::from_entities(child.into(), []);
            join.parts.parent_keys = vec![parent.into()];
            reg.prepare_grow("clusters/t", 0, &[join], store)
        };
        let prepared = fill(&reg, &store, "b", "a").unwrap();
        assert_eq!(prepared.filled, vec![1]);
        for record in &prepared.fills {
            assert_eq!(store.apply(record, 0), 0);
        }
        let refused = fill(&reg, &store, "a", "c").unwrap_err();
        assert_eq!(
            refused,
            RegistryError::Cycle {
                layer: "clusters/t".into(),
                level: 0,
                cycle: vec!["a".into(), "c".into(), "b".into(), "a".into()],
            }
        );
        assert!(store.get("clusters/t", 0, 0).unwrap().parents.is_empty());

        // Across a `PUT`: a new artifact under a held one, and the held one filled under the new.
        let mark = alloc.low_water();
        let mut a_under_d = under("a", &["d"]);
        a_under_d.members = croaring::Bitmap::new();
        let refused = reg
            .prepare_put(
                "clusters/t",
                0,
                &[under("d", &["a"]), a_under_d],
                &store,
                &mut alloc,
            )
            .unwrap_err();
        assert!(
            matches!(refused, RegistryError::Cycle { .. }),
            "the walk sees the batch's edge and the fill together: {refused:?}"
        );
        assert_eq!(alloc.low_water(), mark, "the refusal spends nothing");
        assert_eq!(store.next_ordinal("clusters/t", 0), 3);
    }

    /// **The parts a `PATCH` may fill, each once**: an attachment and a content, beside the
    /// parent above, with the growth's receipt saying how many were filled.
    #[test]
    fn a_growth_fills_an_attachment_and_a_content_once_each() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let mut labels = described("topics/x");
        labels.depends_on = vec!["clusters/a".into()];
        register(&mut reg, &mut alloc, labels).unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c0", &[1, 2, 3])],
        )
        .unwrap();
        // A label published attached (its layer requires it) and without content.
        let mut label = incoming("l0", &[1, 2]);
        label.attached_to = Some(crate::membership::IncomingAttachment {
            layer: "clusters/a".into(),
            level: 0,
            key: "c0".into(),
        });
        let put = reg
            .prepare_put("topics/x", 0, &[label], &store, &mut alloc)
            .unwrap();
        assert_eq!(put.without_content, 1);
        apply_put(&mut reg, &mut store, &put);

        let mut join = crate::membership::IncomingGrowth::from_entities("l0".into(), []);
        join.parts.attached_to = Some(crate::membership::IncomingAttachment {
            layer: "clusters/a".into(),
            level: 0,
            key: "c0".into(),
        });
        join.parts.contents = vec![(0, vec!["shipping".into()])];
        let prepared = reg
            .prepare_grow("topics/x", 0, &[join.clone()], &store)
            .unwrap();
        assert_eq!(
            prepared.filled,
            vec![1],
            "the attachment is held identically; the content fills"
        );
        assert!(prepared.growth.is_none());
        assert!(matches!(
            part_of(&prepared.fills[0]),
            crate::wal::ArtifactPart::Content { rank: 0, .. }
        ));
        for record in &prepared.fills {
            assert_eq!(store.apply(record, 0), 0);
        }
        let again = reg.prepare_grow("topics/x", 0, &[join], &store).unwrap();
        assert!(again.fills.is_empty() && again.growth.is_none());
        assert_eq!(again.filled, vec![0]);

        // A second rank on the same, unpacked artifact fills; a rank past the next is refused.
        let mut next = crate::membership::IncomingGrowth::from_entities("l0".into(), []);
        next.parts.contents = vec![(2, vec!["far".into()])];
        let refused = reg
            .prepare_grow("topics/x", 0, &[next], &store)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::Content { detail, .. } if detail.contains("past the next one, 1")),
            "{refused}"
        );
        let mut next = crate::membership::IncomingGrowth::from_entities("l0".into(), []);
        next.parts.contents = vec![(1, vec!["near".into()])];
        let prepared = reg.prepare_grow("topics/x", 0, &[next], &store).unwrap();
        assert_eq!(prepared.filled, vec![1]);
        for record in &prepared.fills {
            assert_eq!(store.apply(record, 0), 0);
        }
        assert_eq!(store.get("topics/x", 0, 0).unwrap().contents.len(), 2);

        // Once the artifact's content row is packed, a further rank cannot be written (T3).
        store.mark_published("topics/x", 0, 1);
        store.mark_content_published();
        let mut third = crate::membership::IncomingGrowth::from_entities("l0".into(), []);
        third.parts.contents = vec![(2, vec!["third".into()])];
        let refused = reg
            .prepare_grow("topics/x", 0, &[third], &store)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::Content { detail, .. } if detail.contains("durable extent")),
            "{refused}"
        );

        // An attachment the artifact holds differently is the conflict, by name.
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/a",
            &[incoming("c1", &[4])],
        )
        .unwrap();
        let mut moved = crate::membership::IncomingGrowth::from_entities("l0".into(), []);
        moved.parts.attached_to = Some(crate::membership::IncomingAttachment {
            layer: "clusters/a".into(),
            level: 0,
            key: "c1".into(),
        });
        let refused = reg
            .prepare_grow("topics/x", 0, &[moved], &store)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::PartConflict { part, .. } if part == "attached_to"),
            "{refused:?}"
        );
        assert!(!refused.to_string().contains("c0"), "{refused}");
    }

    /// **A content fill on a layer whose content requires every member visible is refused**: a
    /// fill carries no generating set, a set moving only by a page at its rank, and a content
    /// served on an empty set is served to everyone. The refusal is the one a publication with an
    /// empty set draws.
    #[test]
    fn a_content_fill_needing_a_generating_set_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        let mut layer = described("topics/all");
        layer.content.supplied[0].require_member_visibility =
            tessera_types::layer::SuppliedRequirement::All;
        register(&mut reg, &mut alloc, layer).unwrap();
        let put = reg
            .prepare_put("topics/all", 0, &[incoming("t0", &[1])], &store, &mut alloc)
            .unwrap();
        apply_put(&mut reg, &mut store, &put);
        let mut join = crate::membership::IncomingGrowth::from_entities("t0".into(), []);
        join.parts.contents = vec![(0, vec!["x".into()])];
        let refused = reg
            .prepare_grow("topics/all", 0, &[join], &store)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::Content { detail, .. } if detail.contains("carries no generating set")),
            "{refused}"
        );
    }

    /// **A key named twice in one batch with a fixed part on either row is refused at both
    /// routes**, naming the key and before anything is appended: two rows filling one part would
    /// each read it as absent, and the second record would fail at apply after the ack. A `PATCH`
    /// repeating a key with members alone stays a join twice.
    #[test]
    fn a_key_repeated_in_one_batch_with_a_fixed_part_is_refused_at_both_routes() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(
            &mut reg,
            &mut alloc,
            treed("clusters/t", HierarchyKind::Nested),
        )
        .unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/t",
            &[under("a", &[]), under("b", &[]), under("k", &[])],
        )
        .unwrap();

        let mark = alloc.low_water();
        let refused = reg
            .prepare_put(
                "clusters/t",
                0,
                &[under("k", &["a"]), under("k", &["b"])],
                &store,
                &mut alloc,
            )
            .unwrap_err();
        assert_eq!(
            refused,
            RegistryError::RepeatedKey {
                layer: "clusters/t".into(),
                key: "k".into(),
            }
        );
        // The same with one row carrying the part and the other members alone, and with the
        // repeated key new rather than held.
        let mut members_only = incoming("k", &[5]);
        members_only.parent_keys = Vec::new();
        let refused = reg
            .prepare_put(
                "clusters/t",
                0,
                &[under("k", &["a"]), members_only],
                &store,
                &mut alloc,
            )
            .unwrap_err();
        assert!(
            matches!(refused, RegistryError::RepeatedKey { .. }),
            "{refused:?}"
        );
        let refused = reg
            .prepare_put(
                "clusters/t",
                0,
                &[under("n", &["a"]), under("n", &["b"])],
                &store,
                &mut alloc,
            )
            .unwrap_err();
        assert!(
            matches!(refused, RegistryError::RepeatedKey { .. }),
            "{refused:?}"
        );
        assert_eq!(
            alloc.low_water(),
            mark,
            "refused before anything was allocated"
        );
        assert!(store.get("clusters/t", 0, 2).unwrap().parents.is_empty());
        assert_eq!(store.next_ordinal("clusters/t", 0), 3);

        let mut first = crate::membership::IncomingGrowth::from_entities("k".into(), []);
        first.parts.parent_keys = vec!["a".into()];
        let mut second = crate::membership::IncomingGrowth::from_entities("k".into(), []);
        second.parts.parent_keys = vec!["b".into()];
        let refused = reg
            .prepare_grow("clusters/t", 0, &[first.clone(), second], &store)
            .unwrap_err();
        assert_eq!(
            refused,
            RegistryError::RepeatedKey {
                layer: "clusters/t".into(),
                key: "k".into(),
            }
        );
        let members =
            crate::membership::IncomingGrowth::from_entities("k".into(), [EntityId::new(9)]);
        let refused = reg
            .prepare_grow("clusters/t", 0, &[first, members.clone()], &store)
            .unwrap_err();
        assert!(
            matches!(refused, RegistryError::RepeatedKey { .. }),
            "{refused:?}"
        );

        // Members alone, twice: lawful, and a join twice.
        let prepared = reg
            .prepare_grow("clusters/t", 0, &[members.clone(), members], &store)
            .unwrap();
        assert!(prepared.fills.is_empty());
        assert!(prepared.growth.is_some());
    }

    /// **A generating set beside a content fill is refused**, whatever the layer's requirement:
    /// a fill carries no set, a set moving by a page at its rank, and dropping one the caller supplied
    /// would serve the content against a set they did not declare. The same set on a new key is
    /// the publication's own `422`, so the two doors agree.
    #[test]
    fn a_generating_set_beside_a_content_fill_is_refused_rather_than_dropped() {
        let mut reg = LayerRegistry::new();
        let mut store = ArtifactStore::new();
        let mut alloc = Allocator::new(0);
        register(&mut reg, &mut alloc, described("topics/t")).unwrap();
        let put = reg
            .prepare_put("topics/t", 0, &[incoming("t0", &[1])], &store, &mut alloc)
            .unwrap();
        apply_put(&mut reg, &mut store, &put);

        let mut with_set = incoming("t0", &[]);
        with_set.contents = vec![crate::membership::IncomingContent::new(
            vec!["a topic".into()],
            [EntityId::new(1)],
        )];
        let refused = reg
            .prepare_put("topics/t", 0, &[with_set], &store, &mut alloc)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::Content { detail, .. } if detail.contains("a content fill carries none")),
            "{refused:?}"
        );
        assert!(store.get("topics/t", 0, 0).unwrap().contents.is_empty());

        let mut new_with_set = incoming("t1", &[2]);
        new_with_set.contents = vec![crate::membership::IncomingContent::new(
            vec!["a topic".into()],
            [EntityId::new(2)],
        )];
        let refused = reg
            .prepare_put("topics/t", 0, &[new_with_set], &store, &mut alloc)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::Content { detail, .. } if detail.contains("declares a generating set, and none of this layer's content requires")),
            "{refused:?}"
        );
    }

    /// A group-scoped layer, whose artifacts are a set per view of the group.
    fn scoped(name: &str, group: &str) -> LayerDeclaration {
        let mut d = declaration(name);
        d.scope = tessera_types::layer::LayerScope::Group(group.into());
        d
    }

    fn in_view(key: &str, view: &str, members: &[u32]) -> IncomingArtifact {
        let mut artifact = incoming(key, members);
        artifact.view = Some(view.into());
        artifact
    }

    /// **`view` is part of the identity** (`ingest.md` §1.5, `views.md` §3.5): required on a
    /// group-scoped layer, refused on an entity-scoped one, and the same key in two views is two
    /// artifacts with two ordinals and two entities.
    #[test]
    fn the_view_is_required_refused_and_makes_one_key_two_artifacts() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        register(&mut reg, &mut alloc, scoped("clusters/q", "quarter")).unwrap();
        register(&mut reg, &mut alloc, declaration("clusters/one")).unwrap();

        // Absent where the layer is a set per view.
        let refused = reg
            .prepare_put("clusters/q", 0, &[incoming("c1", &[1])], &store, &mut alloc)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::ViewIdentity { detail, .. } if detail.contains("scoped to the group 'quarter'")),
            "{refused:?}"
        );
        // Named where the layer has one set.
        let refused = reg
            .prepare_put(
                "clusters/one",
                0,
                &[in_view("c1", "q1", &[1])],
                &store,
                &mut alloc,
            )
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::ViewIdentity { detail, .. } if detail.contains("entity-scoped")),
            "{refused:?}"
        );

        // One key, two views, two artifacts — in one batch, which is also where a duplicate key
        // would have been refused had the view not been part of the identity.
        let prepared = reg
            .prepare_put(
                "clusters/q",
                0,
                &[in_view("c1", "q1", &[1, 2]), in_view("c1", "q2", &[3])],
                &store,
                &mut alloc,
            )
            .unwrap();
        assert_eq!(prepared.created, 2);
        assert_ne!(prepared.entities[0], prepared.entities[1]);
        apply_put(&mut reg, &mut store, &prepared);
        assert_eq!(
            store.ordinal_of_key("clusters/q", 0, Some("q1"), "c1"),
            Some(0)
        );
        assert_eq!(
            store.ordinal_of_key("clusters/q", 0, Some("q2"), "c1"),
            Some(1)
        );
        assert_eq!(store.ordinal_of_key("clusters/q", 0, None, "c1"), None);
        assert_eq!(
            store.get("clusters/q", 0, 0).unwrap().view.as_deref(),
            Some("q1")
        );
        // Each is served only in its own view: the level projected for q2 holds one artifact.
        assert_eq!(store.level_in_view("clusters/q", 0, "q2").count(), 1);

        // A re-`PUT` of the same key in the same view is the held artifact, not a third.
        let prepared = reg
            .prepare_put(
                "clusters/q",
                0,
                &[in_view("c1", "q1", &[1, 2])],
                &store,
                &mut alloc,
            )
            .unwrap();
        assert_eq!(prepared.created, 0);
        assert_eq!(prepared.joined, 0);
    }

    /// **An edge may not cross views** (`views.md` §3.5): a parent this level holds in another
    /// view is refused naming the crossing, and the same key in the child's own view resolves.
    #[test]
    fn a_parent_in_another_view_is_refused_and_the_one_in_this_view_resolves() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        let mut d = scoped("clusters/q", "quarter");
        d.hierarchy.kind = HierarchyKind::Nested;
        register(&mut reg, &mut alloc, d).unwrap();

        let prepared = reg
            .prepare_put(
                "clusters/q",
                0,
                &[in_view("root", "q1", &[1])],
                &store,
                &mut alloc,
            )
            .unwrap();
        apply_put(&mut reg, &mut store, &prepared);

        let mut child = in_view("leaf", "q2", &[2]);
        child.parent_keys = vec!["root".into()];
        let refused = reg
            .prepare_put("clusters/q", 0, &[child], &store, &mut alloc)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::CrossViewEdge { held_in, .. } if held_in == &["q1".to_string()]),
            "{refused:?}"
        );

        let mut child = in_view("leaf", "q1", &[2]);
        child.parent_keys = vec!["root".into()];
        let prepared = reg
            .prepare_put("clusters/q", 0, &[child], &store, &mut alloc)
            .unwrap();
        apply_put(&mut reg, &mut store, &prepared);
        assert_eq!(
            store.get("clusters/q", 0, 1).unwrap().parents,
            vec![crate::wal::ParentRef {
                level: 0,
                ordinal: 0
            }]
        );
    }

    /// **A second `excluding` on a held key is refused** (`ingest.md` §1.3): the complement it
    /// asks for is taken over the entities that exist now, so it is a different set from the one
    /// the artifact holds rather than the same one said twice.
    #[test]
    fn a_second_exclusion_on_a_held_key_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let prepared = reg
            .prepare_put(
                "clusters/a",
                0,
                &[incoming("c1", &[1, 2, 3])],
                &store,
                &mut alloc,
            )
            .unwrap();
        apply_put(&mut reg, &mut store, &prepared);

        let mut again = incoming("c1", &[]);
        again.exclude([EntityId::new(4)]);
        let refused = reg
            .prepare_put("clusters/a", 0, &[again], &store, &mut alloc)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::ExclusionOnHeldKey { key, .. } if key == "c1"),
            "{refused:?}"
        );
    }

    /// A growth naming no view on a group-scoped layer is refused rather than resolving one
    /// view's artifact by accident.
    #[test]
    fn a_growth_naming_no_view_on_a_group_scoped_layer_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let store = ArtifactStore::default();
        register(&mut reg, &mut alloc, scoped("clusters/q", "quarter")).unwrap();
        let refused = reg
            .resolve_growth_key("clusters/q", 0, None, "c1", &store)
            .unwrap_err();
        assert!(
            matches!(&refused, RegistryError::ViewIdentity { .. }),
            "{refused:?}"
        );
    }

    /// A member key on a group-scoped layer resolves inside the view it names, and on an open
    /// layer a key that view does not hold is minted there, whatever another view holds.
    #[test]
    fn a_member_key_resolves_in_its_own_view_on_a_group_scoped_layer() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        let mut d = scoped("clusters/q", "quarter");
        d.value_set = tessera_types::layer::ValueSet::Open;
        register(&mut reg, &mut alloc, d).unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/q",
            &[in_view("c1", "q1", &[1])],
        )
        .unwrap();

        let held = reg.resolve_or_mint("clusters/q", 0, Some("q1"), "c1", &store);
        assert!(matches!(held, Ok(Some(_))), "{held:?}");
        let minted = reg.resolve_or_mint("clusters/q", 0, Some("q2"), "c1", &store);
        assert!(matches!(minted, Ok(None)), "{minted:?}");
        let refused = reg.resolve_or_mint("clusters/q", 0, None, "c1", &store);
        assert!(
            matches!(refused, Err(RegistryError::ViewIdentity { .. })),
            "{refused:?}"
        );
    }

    /// A growth on a group-scoped layer reaches the artifact in the view it names, and the same
    /// key in another view is another artifact.
    #[test]
    fn a_growth_reaches_the_artifact_in_the_view_it_names() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        register(&mut reg, &mut alloc, scoped("clusters/q", "quarter")).unwrap();
        publish(
            &mut reg,
            &mut store,
            &mut alloc,
            "clusters/q",
            &[in_view("c1", "q1", &[1]), in_view("c1", "q2", &[2])],
        )
        .unwrap();
        let mut join = crate::membership::IncomingGrowth::from_entities(
            "c1".into(),
            [EntityId::new(7)],
        );
        join.view = Some("q2".into());
        let prepared = reg.prepare_grow("clusters/q", 0, &[join], &store).unwrap();
        let record = prepared.growth.expect("a join");
        assert_eq!(store.apply(&record, 0), 0);
        let q1 = store.ordinal_of_key("clusters/q", 0, Some("q1"), "c1").unwrap();
        let q2 = store.ordinal_of_key("clusters/q", 0, Some("q2"), "c1").unwrap();
        assert!(!store.get("clusters/q", 0, q1).unwrap().members.contains(7));
        assert!(store.get("clusters/q", 0, q2).unwrap().members.contains(7));
    }

    /// A label on a layer whose `artifact_visibility` names no field is refused at publication
    /// and at a fill, before anything is allocated.
    #[test]
    fn an_access_label_on_a_layer_naming_no_field_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        register(&mut reg, &mut alloc, declaration("clusters/a")).unwrap();
        let mut labelled = incoming("c1", &[1]);
        labelled.access = vec![b"team-a".to_vec()];
        let refused = reg
            .prepare_put("clusters/a", 0, std::slice::from_ref(&labelled), &store, &mut alloc)
            .unwrap_err();
        assert!(matches!(refused, RegistryError::Access { .. }), "{refused:?}");

        publish(&mut reg, &mut store, &mut alloc, "clusters/a", &[incoming("c1", &[1])]).unwrap();
        let refused = reg
            .prepare_put("clusters/a", 0, &[labelled], &store, &mut alloc)
            .unwrap_err();
        assert!(matches!(refused, RegistryError::Access { .. }), "{refused:?}");
    }

    /// On a layer naming a field, a label is published in canonical order, filled once on a held
    /// artifact that has none, accepted again unchanged, and a different one is a part conflict.
    #[test]
    fn an_access_label_is_published_and_filled_on_the_fill_rule() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        let mut d = declaration("clusters/a");
        d.artifact_visibility = tessera_types::layer::ArtifactVisibility::carried("team");
        register(&mut reg, &mut alloc, d).unwrap();

        let mut first = incoming("c1", &[1]);
        first.access = vec![b"b".to_vec(), b"a".to_vec(), b"b".to_vec()];
        let prepared = reg
            .prepare_put("clusters/a", 0, &[first, incoming("c2", &[2])], &store, &mut alloc)
            .unwrap();
        apply_put(&mut reg, &mut store, &prepared);
        assert_eq!(
            store.get("clusters/a", 0, 0).unwrap().access,
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert!(store.get("clusters/a", 0, 1).unwrap().access.is_empty());

        let mut fill = incoming("c2", &[]);
        fill.access = vec![b"c".to_vec()];
        let prepared = reg
            .prepare_put("clusters/a", 0, std::slice::from_ref(&fill), &store, &mut alloc)
            .unwrap();
        assert!(matches!(
            part_of(&prepared.fills[0]),
            crate::wal::ArtifactPart::Access(_)
        ));
        apply_put(&mut reg, &mut store, &prepared);
        assert_eq!(store.get("clusters/a", 0, 1).unwrap().access, vec![b"c".to_vec()]);

        let again = reg
            .prepare_put("clusters/a", 0, std::slice::from_ref(&fill), &store, &mut alloc)
            .unwrap();
        assert!(again.fills.is_empty());

        let mut other = incoming("c2", &[]);
        other.access = vec![b"d".to_vec()];
        let refused = reg
            .prepare_put("clusters/a", 0, &[other], &store, &mut alloc)
            .unwrap_err();
        assert!(matches!(refused, RegistryError::PartConflict { .. }), "{refused:?}");
    }

    /// A label longer than a stored record can hold, or more labels than it can count, is refused
    /// at publication and at a fill, rather than packed into a record the next open cannot read.
    #[test]
    fn an_access_label_too_long_to_store_is_refused() {
        let mut reg = LayerRegistry::new();
        let mut alloc = Allocator::new(0);
        let mut store = ArtifactStore::default();
        let mut d = declaration("clusters/a");
        d.artifact_visibility = tessera_types::layer::ArtifactVisibility::carried("team");
        register(&mut reg, &mut alloc, d).unwrap();
        publish(&mut reg, &mut store, &mut alloc, "clusters/a", &[incoming("held", &[1])]).unwrap();

        let long = vec![b'x'; u16::MAX as usize];
        let many: Vec<Vec<u8>> = (0..u16::MAX as u32).map(|i| i.to_le_bytes().to_vec()).collect();
        for access in [vec![long.clone()], many.clone()] {
            let mut fresh = incoming("c1", &[1]);
            fresh.access = access.clone();
            assert!(reg
                .prepare_put("clusters/a", 0, &[fresh], &store, &mut alloc)
                .is_err());
            let mut fill = incoming("held", &[]);
            fill.access = access;
            assert!(reg
                .prepare_put("clusters/a", 0, &[fill], &store, &mut alloc)
                .is_err());
        }
        let mut fits = incoming("c2", &[1]);
        fits.access = vec![vec![b'x'; u16::MAX as usize - 1]];
        assert!(reg.prepare_put("clusters/a", 0, &[fits], &store, &mut alloc).is_ok());
    }
}
