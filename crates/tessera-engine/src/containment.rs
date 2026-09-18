//! The containment partition: `G ⊆ M_auth` answered from terms, once for every principal there
//! will ever be.
//!
//! # What it replaces, and why the replacement is exact
//!
//! Containment is the question *does this viewer hold every member of this content's generating
//! set*. [`crate::artifacts::ArtifactRows::satisfied_rank`] answers it by intersecting the
//! projected set with the composed mask, per artifact, per request. `annotations.md` §4 says why
//! that is more work than the question needs:
//!
//! > what decides is **which** terms, never *how many* items
//!
//! An entity is in `M_auth` exactly when one of its own terms is one the principal satisfies, so
//!
//! ```text
//! G ⊆ M_auth   ⟺   ⋀ over e in G of ( ⋁ over t in signature(e) of satisfied(t) )
//! ```
//!
//! — a conjunction of disjunctions over term ids, **with nothing about the mask in it**. Composed
//! once where row forms are built, canonicalised and interned, it is shared by every artifact that
//! draws its generating set from the same signatures and by every token that will ever ask
//! (`design/artifact-serving-at-scale.md` §4.2).
//!
//! **The equivalence is between two whole tests, not two halves**, and it holds in three parts:
//!
//! 1. **Projection loss is checked first and is not in the expression.** A generating set holding a
//!    member still in the commit buffer projects to fewer rows than the record declares, and can
//!    never be contained — see [`crate::artifacts::ArtifactRecords`]. That test is per view, so it
//!    stays on the row form; the expression is view-independent and says nothing about it.
//! 2. **The fragment is exactly the terms, over the base rows.** `M_auth`'s base is `⋃ postings(t)`
//!    over the satisfied terms, so a base-row entity is in it precisely when its signature meets
//!    the principal's term set. The composer reads the build's postings and no delta tier, so it
//!    knows the signature of an entity the build read and not of one that arrived by ingest. A
//!    generating set is projected over the whole row space, and an ordinal one of whose sets holds
//!    a row at or above `base_rows` therefore **declines** here: the row form checks it before
//!    asking an expression ([`crate::artifacts::ArtifactRows::satisfied_rank_via`]) and the exact
//!    masked-count route answers. Composing over the delta tiers would make the partition cover
//!    those ordinals too; that is unmeasured and not built.
//! 3. **The deny correction is the acceptance test, not a refinement** (`design/artifact-serving-at-scale.md`
//!    §4.2; the review's finding 2). A deletion or a suppression removes a member from `M_auth`
//!    whatever the terms say, so an expression consulted alone is **fail-open for exactly the case
//!    the write cycle exists to make safe**. It is answered live per request, from the same
//!    `minus` the composed mask carries — see [`crate::compose::MaskedSet::withholds_any`] — which
//!    is re-derived at the deny's acknowledgement and, on an unsuppress, **re-derived rather than
//!    subtracted**, so `delete → suppress → unsuppress` leaves the entity deleted.
//!
//! ⊘ **The Σ|G| inverted index is deferred and this is the reason.** §4.2 proposes an
//! entity → artifacts index so a deny can correct the partition at the acknowledgement; it is in
//! tension with `annotation-write-cycle.md` §4.5, whose whole content is that the deny lane does
//! no artifact work, and reconciling the two is not this stage's. Evaluating the correction live
//! per **served candidate** is correct without it and is viewport-bounded: it costs one bitmap
//! intersection over a generating set, for the artifacts a viewport actually reaches.
//!
//! # Where it is sound, and where it is not built
//!
//! The expression is over **term signatures**, which is sound exactly when authorisation is
//! signature-shaped: an entity is visible iff its own term set meets the principal's. That is true
//! of the builtin plugin by construction — `build_fragment` unions postings over the satisfied
//! terms and does nothing else — and **unverifiable for a foreign one**, which is
//! `2026-08-21-artifact-layout-selection.md` §9's constraint 10 and reaches **I5** and **I6**. A
//! plugin whose answer depends on something other than the satisfied term set breaks the
//! equivalence the whole structure rests on, and no plugin exists to test it against.
//!
//! Settled fail-closed: **under any plugin but the builtin the partition is not built at all**,
//! and containment stays on the masked-count route, which asks `M_auth` itself and so cannot
//! depend on the shape of the rule that produced it. [`signature_shaped`] is the gate,
//! `Engine::open` says so in the log, and the flag is read where the partition is built rather
//! than cached, so a generation carrying a different manifest cannot inherit a decision made for
//! an earlier one.
//!
//! # What is canonical, and what is not
//!
//! Two expressions are equal when their canonical encodings are equal: each clause's terms sorted
//! ascending and deduplicated, the clauses sorted and deduplicated. That is a **syntactic**
//! canonicalisation. Absorption is not applied — a clause that is a superset of another is
//! implied by it and could be dropped — because it costs O(clauses²) at composition and buys only
//! sharing, never correctness: two expressions that fail to intern together give the same answer
//! twice rather than a wrong answer once.
//!
//! An **empty clause** is an entity in no posting at all: unsatisfiable, and it makes the whole
//! expression unsatisfiable, which is the fail-closed reading and matches what the masked-count
//! route does with a member no term reaches. An expression with **no clauses** is an empty
//! generating set — `annotations.md`'s corpus-independent content — and is satisfied by everyone,
//! which is what `ContentSet::generated_from` documents an empty set to mean.

use std::cell::RefCell;
use std::io;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::{FxHashMap, FxHashSet};

use tessera_authz::postings::{PostingRef, PostingsReader};
use tessera_lifecycle::membership::ArtifactStore;
use tessera_plugin::Plugin;
use tessera_store::derived::{
    compose_containment, generating_entities, PostingSlice, SignatureIndex,
};
use tessera_store::membership::ContainmentPack;
use tessera_types::TermId;

/// Whether a bundle's declared data plugin is the builtin one, and so whether authorisation is
/// signature-shaped for the purposes above.
///
/// **The manifest's hash is the right thing to read**, not the served plugin's, because
/// `Engine::open` already refuses a bundle whose manifest disagrees with the plugin serving it:
/// the two are equal by the time anything here runs, and the manifest is what a partition build
/// has to hand.
pub fn signature_shaped(manifest_data_plugin_hash: &str) -> bool {
    manifest_data_plugin_hash == tessera_plugin::Passthrough::new().data_plugin_hash()
}

/// What composing a partition needs from the generation, with its gate attached.
///
/// **The gate travels with the inputs rather than being applied at the call sites**, of which
/// there are four. A decision taken separately in four places is one that drifts in one of them,
/// and the one it drifts in serves containment answers under a plugin whose rule nobody checked.
pub struct PartitionSource<'a> {
    /// The base postings — the build's `terms/postings.arrow`, which no flush rewrites.
    pub postings: &'a PostingsReader,
    /// The bundle manifest's declared data plugin. `Engine::open` has already refused a bundle
    /// whose manifest disagrees with the plugin serving it, so this is both.
    pub data_plugin_hash: &'a str,
}

impl PartitionSource<'_> {
    pub fn signature_shaped(&self) -> bool {
        signature_shaped(self.data_plugin_hash)
    }
}

/// Build a [`SignatureIndex`] over `postings` — the one adapter between the postings format and
/// the composer.
///
/// **A type shuffle, not a decode.** `tessera-authz` owns `postings.arrow` and `tessera-store`
/// does not depend on it, so the posting arrives here in whichever of its two shapes it is stored
/// in and is handed straight across; the walk that turns postings into signatures is written once,
/// beside the containment format. `tessera build` holds the identical six lines, which is what a
/// crate boundary costs when neither side may depend on the other.
pub fn signature_index(wanted: &Bitmap, postings: &PostingsReader) -> io::Result<SignatureIndex> {
    SignatureIndex::build(wanted, postings.term_count(), &|term, visit| {
        if let Some(posting) = postings.posting_at(term)? {
            match posting {
                PostingRef::Array(bytes) => visit(PostingSlice::Array(bytes)),
                PostingRef::Roaring(view) => visit(PostingSlice::Roaring(&view)),
            }
        }
        Ok(())
    })
}

/// One level's containment partition: an interned expression per `(artifact, rank)`.
///
/// **Keyed by nothing about a principal**, which is the property that does not depend on how many
/// expressions there turn out to be: one copy serves every token there will ever be, where the
/// per-token structure [decision 0093] deletes would have cost 99–343 ms of setup and ~4 MB each.
///
/// **The in-memory form *is* the durable form** — a [`ContainmentPack`], which is either a mapped
/// file the fold wrote or the same bytes held in a buffer. That is what makes
/// `2026-08-21-artifact-layout-selection.md` §9's constraint 13 a change of backing rather than a
/// second encoder: at ten million artifacts a level's table and column are tens of megabytes, and
/// page cache is reclaimable where an anonymous allocation is an OOM. Both routes go through the
/// same framing checks, so a mapped file cannot be read by rules the composed form was never
/// checked against.
///
/// [decision 0093]: ../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md
#[derive(Debug, Clone)]
pub struct ContainmentPartition {
    /// `Arc` because a level's form is shared by every request that reaches it and cloned by
    /// nothing that means to copy it.
    pack: Arc<ContainmentPack>,
}

impl ContainmentPartition {
    /// Compose one `(layer, level)`'s partition against the base postings.
    ///
    /// **Two walks of the level under one borrow of the store**, which is the one-snapshot rule
    /// (`2026-08-21-artifact-layout-selection.md` §9, constraint 1): the first collects the
    /// entities the generating sets name so the signature pass knows what to look for, the second
    /// composes. A growth landing between them would leave the expression describing a set the
    /// membership no longer has, so both come from the same `&ArtifactStore`.
    pub fn compose(
        store: &ArtifactStore,
        layer: &str,
        level: u32,
        postings: &PostingsReader,
    ) -> io::Result<Self> {
        // **Unfiltered by view, and correctly so**: the partition is addressed by ordinal and
        // rank and is not per row, so an entry for an artifact of another view of the group is
        // never asked about — the row form a request tests against holds that view's ordinals
        // alone (`ArtifactStore::level_in_view`, `views.md` §3.5).
        let contents = |visit: &mut dyn FnMut(u32, &[&Bitmap])| {
            for (ordinal, record) in store.level(layer, level) {
                let generating: Vec<&Bitmap> =
                    record.contents.iter().map(|c| &c.generated_from).collect();
                visit(ordinal, &generating);
            }
        };
        let wanted = generating_entities(&contents);
        let signatures = signature_index(&wanted, postings)?;
        Ok(Self::of_bytes(compose_containment(&contents, &signatures)))
    }

    /// Frame composed bytes into the durable form, and read them back through the same checks a
    /// mapped file takes. **The round trip is not ceremony**: it is what makes the two routes one
    /// reader, so a framing rule can never hold for a file and not for the form a publication
    /// built.
    fn of_bytes(bytes: Vec<u8>) -> Self {
        let pack = ContainmentPack::from_bytes(bytes)
            .expect("a partition this crate just composed frames by construction");
        ContainmentPartition {
            pack: Arc::new(pack),
        }
    }

    /// Open a fold-written partition, mapped in place. A torn or foreign file **refuses** —
    /// see [`ContainmentPack`], and note that the failure it prevents is permissive rather than
    /// absent: an expression read short has clauses nobody has to satisfy.
    pub fn open(path: &std::path::Path) -> tessera_store::Result<Self> {
        Ok(ContainmentPartition {
            pack: Arc::new(ContainmentPack::open(path)?),
        })
    }

    /// The durable bytes — what the fold writes into the prefix beside the membership extents.
    pub fn as_bytes(&self) -> &[u8] {
        self.pack.as_bytes()
    }

    /// How many distinct expressions this level composed to. **The census's number**
    /// (`design/artifact-serving-at-scale.md` §4.2): the vocabulary's under per-term authoring, the
    /// population's when generating sets are drawn across a real signature distribution, and it is
    /// what [`Self::answers`] switches on.
    pub fn expressions(&self) -> usize {
        self.pack.expressions() as usize
    }

    /// How many `(artifact, rank)` pairs the column holds.
    pub fn pairs(&self) -> usize {
        self.pack.pairs() as usize
    }

    /// Bytes per identifier — 2 until the expression count passes `u16::MAX`, then 4.
    pub fn id_width(&self) -> u8 {
        self.pack.id_width()
    }

    /// How many ordinals this partition covers, holes included.
    pub fn len(&self) -> usize {
        self.pack.ordinals() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The identifier at `(ordinal, rank)`, or `None` where the partition has no entry — a hole,
    /// an ordinal past the level, or a rank past the artifact's contents.
    fn id_at(&self, ordinal: u32, rank: usize) -> Option<u32> {
        if ordinal as usize >= self.len() {
            return None;
        }
        let lo = self.pack.at(ordinal as usize) as usize;
        let hi = self.pack.at(ordinal as usize + 1) as usize;
        let index = lo.checked_add(rank)?;
        (index < hi).then(|| self.pack.id(index))
    }

    /// Whether `satisfied` meets every clause of expression `id`.
    ///
    /// **No early exit on a clause the viewer nearly holds**, in the same spirit as
    /// `satisfied_rank`'s: the loop stops at the first clause that fails, which is a fact about the
    /// artifact's own composition and not about how close the viewer came — and the expression is
    /// shared by every principal that reaches it.
    fn satisfied_by(&self, id: u32, satisfied: &FxHashSet<TermId>) -> bool {
        let lo = self.pack.expression_at(id as usize) as usize;
        let clauses = self.pack.word(lo);
        let mut at = lo + 1;
        for _ in 0..clauses {
            let len = self.pack.word(at) as usize;
            let met =
                (at + 1..at + 1 + len).any(|w| satisfied.contains(&TermId::new(self.pack.word(w))));
            if !met {
                return false;
            }
            at += 1 + len;
        }
        true
    }

    /// This principal's answers for a **single** artifact — always the lazy face.
    ///
    /// **The switch below is about how much of a level a request is going to walk, and these
    /// callers walk one ordinal.** The drill-down route resolves one identifier and the dependency
    /// prerequisite resolves one target, so settling the whole expression table would be
    /// whole-population work for one answer — and the dependency route runs *per attached
    /// candidate*, which would make it whole-population work per artifact.
    pub fn answer_for_one<'a>(
        &'a self,
        satisfied: &'a FxHashSet<TermId>,
    ) -> ContainmentAnswers<'a> {
        ContainmentAnswers {
            partition: self,
            satisfied,
            memo: Memo::Sparse(RefCell::new(FxHashMap::default())),
        }
    }

    /// This principal's answers over the whole level, ready to be asked per candidate.
    pub fn answers<'a>(&'a self, satisfied: &'a FxHashSet<TermId>) -> ContainmentAnswers<'a> {
        let memo = if self.expressions() <= dense_limit(self.pairs()) {
            // **The union route, in the form this stage can take.** With few distinct expressions
            // the whole table is cheaper to settle once than to memoise: every candidate that
            // arrives finds its answer already there, and nothing is allocated per ask.
            Memo::Dense(
                (0..self.expressions() as u32)
                    .map(|id| self.satisfied_by(id, satisfied))
                    .collect(),
            )
        } else {
            // **Per-candidate evaluation, memoised, which is the route the census says carries the
            // real corpus.** Where the expression count approaches the artifact count, settling the
            // table is the whole-population work the partition exists to avoid; asking on demand is
            // bounded by the viewport, and the memo makes a repeated expression free without
            // making an untouched one cost anything.
            Memo::Sparse(RefCell::new(FxHashMap::default()))
        };
        ContainmentAnswers {
            partition: self,
            satisfied,
            memo,
        }
    }
}

#[cfg(test)]
impl ContainmentPartition {
    /// A partition over clauses named directly: ordinals, then ranks, then clauses, then terms.
    ///
    /// **Test-only, and it exists so a predicate case is about the predicate.** Composing from
    /// postings would put the signature inversion inside every assertion about which rank is
    /// served; the inversion has its own cases, and `tests/artifact_containment.rs` drives both
    /// together against the masked-count route.
    pub(crate) fn of_clauses(ordinals: &[&[&[&[u32]]]]) -> Self {
        let mut builder = tessera_store::derived::ContainmentBuilder::new();
        for (ordinal, ranks) in ordinals.iter().enumerate() {
            builder.push(
                ordinal as u32,
                ranks
                    .iter()
                    .map(|clauses| tessera_store::derived::encode_expression(clauses.to_vec())),
            );
        }
        Self::of_bytes(builder.finish())
    }
}

/// The width at which settling the whole expression table stops being the cheaper route.
///
/// **A choice with a measurement's shape behind it and not a ruling.** The probe switches its own
/// two routes at `settled + open > max(rows / 64, 4096)`
/// (`design/artifact-serving-at-scale.md` §4.2), and the quantity it is comparing is the same one:
/// how much of the population a request is about to touch, against how much a whole-population
/// pass would cost. Here the population is the level's `(artifact, rank)` pairs, because that is
/// what a request may walk, and the floor keeps a small level on the dense route where the
/// allocation is nothing. It appears in no design document, and a build that picks a different
/// constant measures a different system — recorded at
/// `2026-08-21-artifact-layout-selection.md` §9, constraint 5.
fn dense_limit(pairs: usize) -> usize {
    (pairs / 64).max(4096)
}

enum Memo {
    /// Every expression settled up front.
    Dense(Vec<bool>),
    /// Settled on first ask. `RefCell` because the predicate takes `&self` — one request, one
    /// thread, and the borrow never spans a call out.
    Sparse(RefCell<FxHashMap<u32, bool>>),
}

/// One principal's view of one level's partition.
pub struct ContainmentAnswers<'a> {
    partition: &'a ContainmentPartition,
    satisfied: &'a FxHashSet<TermId>,
    memo: Memo,
}

impl ContainmentAnswers<'_> {
    /// Whether this principal satisfies the expression at `(ordinal, rank)`.
    ///
    /// `None` where the partition has no entry there, which is what puts the caller back on the
    /// masked-count route rather than answering from a structure that does not cover the case.
    pub fn satisfies(&self, ordinal: u32, rank: usize) -> Option<bool> {
        let id = self.partition.id_at(ordinal, rank)?;
        Some(match &self.memo {
            Memo::Dense(settled) => settled[id as usize],
            Memo::Sparse(memo) => {
                if let Some(answer) = memo.borrow().get(&id) {
                    return Some(*answer);
                }
                let answer = self.partition.satisfied_by(id, self.satisfied);
                memo.borrow_mut().insert(id, answer);
                answer
            }
        })
    }

    /// Whether this level's partition covers `ordinal` at all.
    pub fn covers(&self, ordinal: u32) -> bool {
        (ordinal as usize) < self.partition.len()
    }

    /// Whether the whole expression table was settled up front — the two faces, observable so a
    /// test can assert which one a level took. Operator- and test-facing only; it names no
    /// principal and no artifact.
    pub fn settled_eagerly(&self) -> bool {
        matches!(self.memo, Memo::Dense(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn satisfied(terms: &[u32]) -> FxHashSet<TermId> {
        terms.iter().map(|t| TermId::new(*t)).collect()
    }

    /// One artifact per expression, so an ordinal indexes the case being made.
    fn partition_of(expressions: &[&[&[u32]]]) -> ContainmentPartition {
        let ordinals: Vec<&[&[&[u32]]]> = expressions.iter().map(std::slice::from_ref).collect();
        ContainmentPartition::of_clauses(&ordinals)
    }

    fn holds(partition: &ContainmentPartition, ordinal: u32, terms: &[u32]) -> bool {
        let held = satisfied(terms);
        partition
            .answers(&held)
            .satisfies(ordinal, 0)
            .expect("the fixture covers this ordinal")
    }

    /// The whole of the expression's semantics in one case: **every** clause must be met, and one
    /// term anywhere in a clause meets it. A viewer holding most of a generating set's signatures
    /// and missing one is not contained — which is `satisfied_rank`'s rule, restated in terms.
    #[test]
    fn a_conjunction_of_disjunctions_and_not_a_coverage_fraction() {
        let partition = partition_of(&[&[&[1, 2], &[3]]]);
        assert!(holds(&partition, 0, &[1, 3]));
        assert!(holds(&partition, 0, &[2, 3]));
        // Holds one clause entirely and every term of a dozen others: still not contained.
        assert!(!holds(&partition, 0, &[1, 2, 4, 5, 6, 7, 8, 9]));
        assert!(!holds(&partition, 0, &[3]));
    }

    /// An empty clause is a member no term reaches, and it makes the expression unsatisfiable for
    /// everybody — the fail-closed direction, and the same answer the masked-count route gives a
    /// member outside every posting.
    #[test]
    fn a_member_no_term_reaches_contains_nobody() {
        let partition = partition_of(&[&[&[1], &[]]]);
        assert!(!holds(&partition, 0, &[1]));
        assert!(!holds(&partition, 0, &[0, 1, 2, 3, 4, 5]));
    }

    /// No clauses at all is an **empty generating set** — corpus-independent content — and it is
    /// satisfied by everyone, including a principal holding nothing. Collapsing this with the case
    /// above would withhold every corpus-independent label from every viewer.
    #[test]
    fn an_empty_generating_set_contains_everyone() {
        let partition = partition_of(&[&[]]);
        assert!(holds(&partition, 0, &[]));
    }

    /// Interning is by canonical form, so two generating sets drawn from the same signatures share
    /// one identifier however differently they were written down — which is the whole of the
    /// sharing the design counts on where a layer is authored per term.
    #[test]
    fn the_same_expression_written_two_ways_interns_once() {
        let partition = partition_of(&[&[&[1, 2], &[3]], &[&[3], &[1, 2]], &[&[3], &[3], &[1, 2]]]);
        assert_eq!(
            partition.expressions(),
            1,
            "clause order and a repeated clause are not part of the identity"
        );
        assert_eq!(partition.pairs(), 3);
    }

    /// **A `u16` column that would truncate is promoted rather than wrapped.** The failure this
    /// forbids is silent: a wrapped identifier names a *different* expression, so an artifact is
    /// served on another artifact's containment answer.
    #[test]
    fn the_identifier_column_promotes_rather_than_truncating() {
        // One distinct expression per ordinal, past the point a two-byte identifier can address.
        let terms: Vec<Vec<u32>> = (0..=u16::MAX as u32 + 1).map(|t| vec![t]).collect();
        let clauses: Vec<Vec<&[u32]>> = terms.iter().map(|t| vec![t.as_slice()]).collect();
        let ranks: Vec<&[&[u32]]> = clauses.iter().map(Vec::as_slice).collect();
        let ordinals: Vec<&[&[&[u32]]]> = ranks.iter().map(std::slice::from_ref).collect();

        let narrow = ContainmentPartition::of_clauses(&ordinals[..u16::MAX as usize + 1]);
        assert_eq!(narrow.expressions(), u16::MAX as usize + 1);
        assert_eq!(narrow.id_width(), 2, "still addressable by two bytes");

        let wide = ContainmentPartition::of_clauses(&ordinals);
        assert_eq!(wide.expressions(), u16::MAX as usize + 2);
        assert_eq!(
            wide.id_width(),
            4,
            "the column widened rather than wrapping"
        );
        // The last two ordinals name *different* expressions, which is exactly what a wrap would
        // have destroyed.
        assert!(holds(&wide, u16::MAX as u32 + 1, &[u16::MAX as u32 + 1]));
        assert!(!holds(&wide, u16::MAX as u32 + 1, &[0]));
        assert!(holds(&wide, 0, &[0]));
    }

    /// **The durable form is the in-memory form.** A composed partition written to a file and
    /// opened mapped answers identically — which is what makes the fold's consolidation a change
    /// of backing rather than a second encoder.
    #[test]
    fn a_partition_answers_the_same_mapped_as_composed() {
        let composed = partition_of(&[&[&[1, 2], &[3]], &[&[7]], &[]]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("partition.tscp");
        std::fs::write(&path, composed.as_bytes()).unwrap();
        let mapped = ContainmentPartition::open(&path).unwrap();

        assert_eq!(mapped.len(), composed.len());
        assert_eq!(mapped.pairs(), composed.pairs());
        assert_eq!(mapped.expressions(), composed.expressions());
        for terms in [&[1u32, 3][..], &[3], &[7], &[]] {
            for ordinal in 0..3u32 {
                assert_eq!(
                    holds(&mapped, ordinal, terms),
                    holds(&composed, ordinal, terms),
                    "the mapped partition disagrees at ordinal {ordinal} for {terms:?}"
                );
            }
        }
    }

    /// A file that is not a partition, or is one written by a different packer, **refuses**. The
    /// framing lives in `tessera_store::membership`, which has the exhaustive cases; this pins
    /// that the engine's opener goes through them rather than around.
    #[test]
    fn a_torn_partition_file_refuses_rather_than_opening_short() {
        let composed = partition_of(&[&[&[1, 2], &[3]]]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("torn.tscp");
        let mut bytes = composed.as_bytes().to_vec();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&path, &bytes).unwrap();
        assert!(ContainmentPartition::open(&path).is_err());
    }

    /// The two faces answer identically; only the work differs. A level whose expressions are few
    /// settles the table, one whose expressions approach its pairs settles them on demand — and a
    /// route that disagreed with the other would be two transcriptions of one rule.
    #[test]
    fn both_faces_give_the_same_answer() {
        assert_eq!(dense_limit(0), 4096);
        assert_eq!(dense_limit(1_000_000), 15_625);
        let partition = partition_of(&[&[&[1]], &[&[2]], &[&[1], &[2]]]);
        let held = satisfied(&[1]);
        let dense = partition.answers(&held);
        assert!(dense.settled_eagerly());
        let sparse = ContainmentAnswers {
            partition: &partition,
            satisfied: &held,
            memo: Memo::Sparse(RefCell::new(FxHashMap::default())),
        };
        for ordinal in 0..3u32 {
            assert_eq!(
                dense.satisfies(ordinal, 0),
                sparse.satisfies(ordinal, 0),
                "the two faces disagree at ordinal {ordinal}"
            );
        }
        assert_eq!(dense.satisfies(0, 0), Some(true));
        assert_eq!(dense.satisfies(1, 0), Some(false));
        assert_eq!(dense.satisfies(2, 0), Some(false));
        assert_eq!(dense.satisfies(0, 1), None, "no second rank to answer for");
        assert_eq!(dense.satisfies(9, 0), None, "past the level");
    }
}
