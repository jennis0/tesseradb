//! **The containment partition answers what the mask answers, or it is a disclosure.**
//!
//! `crate::containment` replaces a per-request intersection with a build-time boolean expression
//! over term signatures. The replacement is only worth anything if it is exact, and it is exact
//! only under three conditions the module argues for and this file refuses to take on trust:
//! projection loss is checked before the expression, the fragment really is the union of the
//! satisfied terms' postings, and a deletion or suppression corrects the answer **live**. Get the
//! third wrong and the partition serves content generated from documents the viewer may no longer
//! see — the fail-open the review's finding 2 named.
//!
//! This is the engine-side twin of the probe's `assert_containment_partition`, and it is built to
//! fail where the probe's fixture could not. That fixture planted thirty-two signature groups and
//! drew every generating set from inside one, so containment and non-empty masked membership
//! coincided and nothing could disagree (`design/artifact-serving-at-scale.md` §4.1). Here the
//! signatures are many and pseudo-random, the generating sets are drawn **across** them, the row
//! order is a shuffle rather than the entity order, some sets are lossy in projection, and the
//! overlay is live under both arms.

use std::sync::Arc;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::artifacts::{ArtifactProjections, ArtifactRows, Containment};
use tessera_engine::compose::{compose, EffectiveMask};
use tessera_engine::projection::RowProjection;
use tessera_engine::containment::{signature_shaped, ContainmentPartition, PartitionSource};
use tessera_engine::denied_rows_of;
use tessera_lifecycle::membership::{ArtifactRecord, ArtifactStore, ContentSet};
use tessera_lifecycle::{ChangeOp, IngestBuffer, Overlay};
use tessera_store::write::write_permutation;
use tessera_store::{Permutation, RowSpace};
use tessera_types::{EntityId, TermId};

const UNIVERSE: u32 = 4_000;
const TERMS: u32 = 40;
/// Two populations, because the partition has **two faces** and the switch between them is a
/// property of the population rather than of the request (`crate::containment`'s `dense_limit`).
/// The small one settles its whole expression table up front; the large one has more distinct
/// expressions than the switch's floor, so it evaluates per candidate and memoises. Both are run
/// through the same differential, because a face that answered differently would be two
/// transcriptions of one rule.
const SMALL: u32 = 300;
const LARGE: u32 = 5_000;
const LAYER: &str = "clusters/a";
const SMALL_TERM_THRESHOLD: u32 = 32;

/// The entities a flush published: they own **extent rows** above the base, and their terms are in
/// a delta tier rather than in `terms/postings.arrow`.
///
/// A generating set holding one projects whole, so it clears the projection check and reaches the
/// containment test. The partition composed from the base postings knows nothing of these terms,
/// which is why an ordinal whose set reaches above the base rows declines to the masked-count
/// route.
const FLUSHED_LO: u32 = UNIVERSE;
const FLUSHED_HI: u32 = UNIVERSE + 31;
const FLUSHED_ROWS: u32 = FLUSHED_HI - FLUSHED_LO + 1;

/// An entity no row covers — a member **still in the commit buffer**.
///
/// A generating set holding one is lossy in projection and can never be contained, and the case
/// only bites because these entities **do** carry terms: an unlabelled member would already fail
/// the expression, so a fixture that planted one would leave the projection-loss check untested
/// while appearing to cover it.
const UNPROJECTABLE: u32 = UNIVERSE + 40;
const LABELLED: u32 = UNIVERSE + 64;

/// The corpus, its postings, its row space, and the artifacts over it.
struct Fixture {
    _temp: TempDir,
    postings: PostingsReader,
    /// The flushed entities' terms, as a flush writes them: read when a mask is composed and not
    /// by the partition's composer.
    tier: Arc<tessera_authz::DeltaTier>,
    row_space: RowSpace,
    store: ArtifactStore,
    /// `entity → its terms`, kept so the expected answers below are computed from the fixture's
    /// own generator rather than from anything the engine built.
    terms_of: Vec<Vec<u32>>,
    /// Per ordinal, per rank, the generating set as it was published — entity space.
    published: Vec<Vec<Vec<u32>>>,
    artifacts: u32,
}

fn build_fixture() -> Fixture {
    build_fixture_n(SMALL)
}

fn build_fixture_n(artifacts: u32) -> Fixture {
    let temp = TempDir::new().unwrap();
    let mut rng = StdRng::seed_from_u64(0x_c0_de_5e_ed);

    // **Signatures that are many and drawn independently of anything else.** One to three terms
    // per entity out of forty gives thousands of distinct signatures over four thousand entities,
    // which is the census's *drawn* case rather than the fixture's per-term one.
    let terms_of: Vec<Vec<u32>> = (0..LABELLED)
        .map(|_| {
            let mut terms: Vec<u32> = (0..rng.gen_range(1..=3))
                .map(|_| rng.gen_range(0..TERMS))
                .collect();
            terms.sort_unstable();
            terms.dedup();
            terms
        })
        .collect();
    // **The base postings hold the entities the build read; the tier holds the flushed ones.**
    // That split is the whole reason the partition cannot answer for a set that reaches above the
    // base rows, so the fixture may not blur it.
    let mut per_term: Vec<Vec<u32>> = vec![Vec::new(); TERMS as usize];
    let mut per_term_flushed: Vec<Vec<u32>> = vec![Vec::new(); TERMS as usize];
    for (entity, terms) in terms_of.iter().enumerate() {
        for term in terms {
            if (entity as u32) < UNIVERSE {
                per_term[*term as usize].push(entity as u32);
            } else {
                per_term_flushed[*term as usize].push(entity as u32);
            }
        }
    }

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &per_term, SMALL_TERM_THRESHOLD).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();
    let tier_path = temp.path().join("tier.arrow");
    let entries: Vec<(u32, Vec<u32>)> = per_term_flushed
        .into_iter()
        .enumerate()
        .filter(|(_, entities)| !entities.is_empty())
        .map(|(term, entities)| (term as u32, entities))
        .collect();
    tessera_authz::write_delta_tier_at(&tier_path, &entries, SMALL_TERM_THRESHOLD).unwrap();
    let tier = Arc::new(tessera_authz::DeltaTier::open(&tier_path).unwrap());

    // **A shuffle, not the identity.** Row order and entity order agreeing would let a bug that
    // conflated the two pass every case here.
    let mut order: Vec<EntityId> = (0..UNIVERSE as u64).map(EntityId::new).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.gen_range(0..=i));
    }
    let perm_path = temp.path().join("permutation.bin");
    write_permutation(&perm_path, &order, UNIVERSE as u64).unwrap();
    // **One flushed segment above the base**, so an entity that arrived by ingest has a row and a
    // still-buffered one has none. The extent's rows are a shuffle of its own span, as a
    // Morton-sorted segment's are.
    let mut extent_rows: Vec<u32> = (0..FLUSHED_ROWS).collect();
    for i in (1..extent_rows.len()).rev() {
        extent_rows.swap(i, rng.gen_range(0..=i));
    }
    let row_space = RowSpace::new(Arc::new(Permutation::load(&perm_path).unwrap()), UNIVERSE)
        .with_extent(tessera_store::SegmentExtent {
            entity_lo: u64::from(FLUSHED_LO),
            entity_hi: u64::from(FLUSHED_HI),
            seg_id: "s-flush-0".to_string(),
            row_base: UNIVERSE,
            rows: extent_rows,
        })
        .expect("the extent continues row space exactly");

    let mut store = ArtifactStore::new();
    let mut published = Vec::new();
    for ordinal in 0..artifacts {
        // A membership of its own, unrelated to the generating sets below — the artifact's count
        // and its containment are different questions and must not be answerable from each other.
        let members: Bitmap = (0..40).map(|_| rng.gen_range(0..UNIVERSE)).collect();
        let mut sets = Vec::new();
        for rank in 0..rng.gen_range(1..=3u32) {
            let width = match rank {
                // Rank 0 is the widest, so a viewer usually falls through to a narrower one —
                // which is the shape decision 0078 describes and the shape that makes a
                // rank-for-rank comparison mean something.
                0 => rng.gen_range(4..=10),
                _ => rng.gen_range(1..=3),
            };
            let mut set: Vec<u32> = (0..width).map(|_| rng.gen_range(0..UNIVERSE)).collect();
            // One artifact in twenty carries a set that lost a member in projection.
            if ordinal % 20 == 3 && rank == 0 {
                set.push(UNPROJECTABLE);
            }
            // **One artifact in seven draws a member from the flushed segment.** Its set projects
            // whole, so it reaches the containment test and the partition has to decline it.
            if ordinal % 7 == 2 {
                set.push(FLUSHED_LO + (ordinal + rank) % FLUSHED_ROWS);
            }
            set.sort_unstable();
            set.dedup();
            sets.push(set);
        }
        published.push(sets.clone());
        store.put(
            LAYER,
            0,
            ordinal,
            ArtifactRecord {
                entity: EntityId::new(u64::from(u32::MAX - ordinal)),
                key: None,
                view: None,
                incarnation: 0,
                members: members.into(),
                contents: sets
                    .into_iter()
                    .map(|set| ContentSet {
                        values: Some(vec!["text".to_string()]),
                        digest: tessera_lifecycle::membership::content_digest(&[
                            "text".to_string()
                        ]),
                        cardinality: set.len() as u64,
                        generated_from: set.into_iter().collect(),
                    })
                    .collect(),
                attached_to: None,
                parents: Vec::new(),
                access: Vec::new(),
            },
            None,
        );
    }

    Fixture {
        _temp: temp,
        postings,
        tier,
        row_space,
        store,
        terms_of,
        published,
        artifacts,
    }
}

impl Fixture {
    fn rows(&self) -> ArtifactRows {
        ArtifactRows::build(self.store.level(LAYER, 0), &self.row_space)
    }

    fn partition(&self) -> ContainmentPartition {
        ContainmentPartition::compose(&self.store, LAYER, 0, &self.postings)
            .expect("the fixture's postings are readable")
    }

    /// The composed mask, the principal's term set, and this view's deny mask — the three things
    /// a request holds, derived exactly as `compose` and the deny lane derive them.
    fn mask(
        &self,
        granted: &[u32],
        overlay: &Overlay,
    ) -> (EffectiveMask, FxHashSet<TermId>, Bitmap) {
        let satisfied: FxHashSet<TermId> = granted.iter().map(|t| TermId::new(*t)).collect();
        let mut sorted: Vec<TermId> = satisfied.iter().copied().collect();
        sorted.sort_unstable_by_key(|t| t.raw());
        let cache = FragmentCache::new(&self._temp.path().join("frag"), [1u8; 32], [2u8; 32]);
        let fragment = cache
            .get_or_build(
                &sorted,
                &self.postings,
                std::slice::from_ref(&self.tier),
                u64::from(FLUSHED_HI),
            )
            .unwrap();
        let base = Arc::new(RowProjection::walk(&fragment, &self.row_space));
        let buffer = IngestBuffer::new();
        let denied = denied_rows_of(overlay, &self.row_space);
        let buffered = tessera_engine::buffered_rows_of(&buffer, &self.row_space);
        let mask = compose(
            &satisfied,
            overlay,
            &buffer,
            base,
            &self.row_space,
            &denied,
            Some(&buffered),
        );
        (mask, satisfied, denied)
    }

    /// The answer computed from the fixture's own generator: the first rank whose every member
    /// this principal holds and none of whose members is denied, with a lossy set never contained.
    ///
    /// **Independent of both arms**, which is what makes a three-way agreement worth more than the
    /// two-way one: a bug shared by the partition and the mask would still show up here.
    fn expected(&self, ordinal: u32, granted: &[u32], overlay: &Overlay) -> Containment {
        for (rank, set) in self.published[ordinal as usize].iter().enumerate() {
            let contained = set.iter().all(|entity| {
                self.row_space
                    .row_of(EntityId::new(u64::from(*entity)))
                    .is_some()
                    && self.terms_of[*entity as usize]
                        .iter()
                        .any(|t| granted.contains(t))
                    && !overlay.is_deleted(EntityId::new(u64::from(*entity)))
                    && !overlay.is_suppressed(EntityId::new(u64::from(*entity)))
            });
            if contained {
                return Containment::Satisfied(rank as u32);
            }
        }
        Containment::Unsatisfied
    }

    /// Whether any of this ordinal's generating sets holds a member the flush published — the
    /// ordinals the partition declines and the masked-count route answers for.
    fn reaches_flushed(&self, ordinal: u32) -> bool {
        self.published[ordinal as usize]
            .iter()
            .flatten()
            .any(|entity| (FLUSHED_LO..=FLUSHED_HI).contains(entity))
    }

    /// The first ordinal the partition answers for that satisfies `wanted`.
    fn ordinal_where(&self, wanted: impl Fn(u32) -> bool) -> u32 {
        (0..self.artifacts)
            .find(|o| !self.reaches_flushed(*o) && wanted(*o))
            .expect("the fixture holds such an ordinal")
    }
}

/// Six principals of very different breadths, including one holding nothing and one holding
/// everything — the grid the cost inversion is measured over, used here for correctness.
fn principals() -> Vec<Vec<u32>> {
    vec![
        (0..TERMS).collect(),
        (0..TERMS).filter(|t| t % 2 == 0).collect(),
        (0..TERMS).filter(|t| t % 3 == 0).collect(),
        vec![1, 5, 9],
        vec![7],
        Vec::new(),
    ]
}

/// **The stage's headline.** Over a corpus whose signatures are many and whose generating sets are
/// drawn across them, the partition's arm and the masked-count route return the **same rank at
/// every ordinal for every principal**, with deletions and suppressions live — and both agree with
/// an answer computed from the fixture's own generator.
#[test]
fn the_partition_and_the_mask_serve_the_same_rank_at_every_ordinal() {
    // The small population settles its whole table; the large one is past the switch's floor and
    // evaluates per candidate. Both must agree with the mask and with the fixture's own answer.
    for (artifacts, settled_eagerly) in [(SMALL, true), (LARGE, false)] {
        let fx = build_fixture_n(artifacts);
        assert_eq!(
            differential(&fx),
            settled_eagerly,
            "the {artifacts}-artifact population took the other face"
        );
    }
}

/// The whole grid for one population, returning which face its answers took.
fn differential(fx: &Fixture) -> bool {
    let rows = fx.rows();
    let partitioned = fx.rows().with_partition(Some(fx.partition()));
    assert!(
        partitioned.partition().unwrap().expressions() > 100,
        "a fixture whose generating sets all interned together would prove nothing about \
         agreement — the census's drawn case is the one that has to hold"
    );

    // Live denies, in the three shapes that behave differently: a plain suppression, a deletion,
    // and the `delete -> suppress -> unsuppress` sequence, which must leave the entity **deleted**.
    let mut overlay = Overlay::new();
    for entity in (0..UNIVERSE).filter(|e| e % 37 == 0) {
        overlay.apply(EntityId::new(u64::from(entity)), ChangeOp::Suppress);
    }
    for entity in (0..UNIVERSE).filter(|e| e % 53 == 0) {
        overlay.apply(EntityId::new(u64::from(entity)), ChangeOp::Delete);
    }
    for entity in (0..UNIVERSE).filter(|e| e % 101 == 0) {
        let entity = EntityId::new(u64::from(entity));
        overlay.apply(entity, ChangeOp::Delete);
        overlay.apply(entity, ChangeOp::Suppress);
        overlay.apply(entity, ChangeOp::Unsuppress);
    }

    let mut served = 0usize;
    let mut declined = 0usize;
    let mut settled_eagerly = None;
    for overlay in [&Overlay::new(), &overlay] {
        for granted in principals() {
            let (mask, satisfied, denied) = fx.mask(&granted, overlay);
            let answers = partitioned.partition().unwrap().answers(&satisfied);
            settled_eagerly = Some(answers.settled_eagerly());
            for ordinal in 0..fx.artifacts {
                let expected = fx.expected(ordinal, &granted, overlay);
                assert_eq!(
                    rows.satisfied_rank(ordinal, &mask, true),
                    expected,
                    "the masked-count route disagrees with the fixture's own answer at \
                     ordinal {ordinal} for terms {granted:?}"
                );
                let via = partitioned.satisfied_rank_via(ordinal, &answers, &denied, true);
                if fx.reaches_flushed(ordinal) {
                    // **The partition declines and the caller falls back.** Its expression was
                    // composed without the flushed member's terms, so an answer from it would be
                    // `Unsatisfied` for every principal — which is exactly issue #150.
                    assert_eq!(
                        via, None,
                        "an ordinal whose generating set reaches above the base rows was answered \
                         from the partition at ordinal {ordinal} for terms {granted:?}"
                    );
                    declined += 1;
                } else {
                    assert_eq!(
                        via,
                        Some(expected),
                        "the containment partition disagrees at ordinal {ordinal} for terms \
                         {granted:?}"
                    );
                }
                if matches!(expected, Containment::Satisfied(_)) {
                    served += 1;
                }
            }
        }
    }
    assert!(
        declined > 0,
        "no ordinal reached above the base rows, so the declining arm was never exercised"
    );
    // A run where nothing was ever contained would agree trivially.
    assert!(
        served > fx.artifacts as usize,
        "only {served} contained answers across the whole grid — the fixture is not exercising \
         the satisfied path"
    );
    settled_eagerly.expect("the grid ran at least one principal")
}

/// **A generating set holding a flushed member is served, and to the right principals.** Issue
/// #150: such a set used to project short of its declared size, so the content was withheld from
/// everyone. It now projects whole through the extent, and the masked-count route decides it.
#[test]
fn a_set_holding_a_flushed_member_is_served_to_a_viewer_who_holds_every_member() {
    let fx = build_fixture();
    let rows = fx.rows();
    let ordinal = (0..fx.artifacts)
        .find(|o| fx.reaches_flushed(*o))
        .expect("the fixture draws some set from the flushed segment");
    let set = &fx.published[ordinal as usize][0];
    let flushed = *set
        .iter()
        .find(|e| (FLUSHED_LO..=FLUSHED_HI).contains(e))
        .expect("rank 0 holds the flushed member");

    // The set projects to as many rows as the record declares, which is what the withholding gate
    // reads.
    assert_eq!(
        rows.membership().generating(ordinal)[0].cardinality(),
        set.len() as u64,
        "the set lost a member on the way into row space"
    );

    let whole: Vec<u32> = set
        .iter()
        .flat_map(|e| fx.terms_of[*e as usize].clone())
        .collect();
    let overlay = Overlay::new();
    let (mask, _, _) = fx.mask(&whole, &overlay);
    assert_eq!(
        rows.satisfied_rank(ordinal, &mask, true),
        Containment::Satisfied(0),
        "a principal holding every member's terms is served rank 0"
    );

    // The same principal without the flushed member's terms, which no other member of rank 0
    // carries: rank 0 is out of contention.
    let others: Vec<u32> = set
        .iter()
        .filter(|e| **e != flushed)
        .flat_map(|e| fx.terms_of[*e as usize].clone())
        .collect();
    if !fx.terms_of[flushed as usize]
        .iter()
        .any(|t| others.contains(t))
    {
        let (mask, _, _) = fx.mask(&others, &overlay);
        assert_ne!(
            rows.satisfied_rank(ordinal, &mask, true),
            Containment::Satisfied(0),
            "a principal who cannot see the flushed member was served the content generated \
             from it"
        );
    }
}

/// **A denied flushed member takes its rank out of contention, exactly as a base-row member does.**
/// The deny mask is derived over the whole row space, so a suppression reaches an extent row; and
/// an unsuppress re-derives, so `delete → suppress → unsuppress` leaves the member deleted.
#[test]
fn a_suppressed_flushed_member_is_not_contained_and_an_unsuppress_re_derives() {
    let fx = build_fixture();
    let rows = fx.rows();
    let ordinal = (0..fx.artifacts)
        .find(|o| fx.reaches_flushed(*o))
        .expect("the fixture draws some set from the flushed segment");
    let set = &fx.published[ordinal as usize][0];
    let flushed = EntityId::new(u64::from(
        *set.iter()
            .find(|e| (FLUSHED_LO..=FLUSHED_HI).contains(e))
            .expect("rank 0 holds the flushed member"),
    ));
    let granted: Vec<u32> = set
        .iter()
        .flat_map(|e| fx.terms_of[*e as usize].clone())
        .collect();

    let contained = |overlay: &Overlay| {
        let (mask, _, _) = fx.mask(&granted, overlay);
        rows.satisfied_rank(ordinal, &mask, true)
    };
    assert_eq!(contained(&Overlay::new()), Containment::Satisfied(0));

    let mut overlay = Overlay::new();
    overlay.apply(flushed, ChangeOp::Suppress);
    assert_ne!(
        contained(&overlay),
        Containment::Satisfied(0),
        "a suppressed member of the set must take its rank out of contention"
    );

    let mut overlay = Overlay::new();
    overlay.apply(flushed, ChangeOp::Unsuppress);
    assert_eq!(
        contained(&overlay),
        Containment::Satisfied(0),
        "the unsuppress restores the member, and the rank is served again"
    );

    let mut overlay = Overlay::new();
    overlay.apply(flushed, ChangeOp::Delete);
    let deleted = contained(&overlay);
    assert_ne!(deleted, Containment::Satisfied(0));
    overlay.apply(flushed, ChangeOp::Suppress);
    overlay.apply(flushed, ChangeOp::Unsuppress);
    assert_eq!(
        contained(&overlay),
        deleted,
        "the deletion stands through an unsuppress"
    );
}

/// **An unsuppress re-derives; it does not subtract.** `delete → suppress → unsuppress` must leave
/// the entity deleted, so a generating set holding it must stay uncontained — through the
/// partition's arm, which is the one that would otherwise have answered from terms alone.
#[test]
fn an_unsuppress_does_not_restore_a_deleted_member_through_the_partition() {
    let fx = build_fixture();
    let partitioned = fx.rows().with_partition(Some(fx.partition()));
    let granted: Vec<u32> = (0..TERMS).collect();

    // An ordinal whose first rank is contained by a principal holding every term, and which the
    // partition answers for.
    let ordinal = fx.ordinal_where(|o| {
        fx.expected(o, &granted, &Overlay::new()) == Containment::Satisfied(0)
            && fx.published[o as usize].len() > 1
    });
    let member = EntityId::new(u64::from(fx.published[ordinal as usize][0][0]));

    let contained_via = |overlay: &Overlay| {
        let (_mask, satisfied, denied) = fx.mask(&granted, overlay);
        let answers = partitioned.partition().unwrap().answers(&satisfied);
        partitioned
            .satisfied_rank_via(ordinal, &answers, &denied, true)
            .expect("the partition covers this ordinal")
    };

    assert_eq!(contained_via(&Overlay::new()), Containment::Satisfied(0));

    let mut overlay = Overlay::new();
    overlay.apply(member, ChangeOp::Delete);
    let deleted = contained_via(&overlay);
    assert_ne!(
        deleted,
        Containment::Satisfied(0),
        "a deleted member must take its rank out of contention"
    );

    overlay.apply(member, ChangeOp::Suppress);
    overlay.apply(member, ChangeOp::Unsuppress);
    assert_eq!(
        contained_via(&overlay),
        deleted,
        "the unsuppress removed the suppression and re-derived; the deletion stands, so the rank \
         is still out of contention"
    );
}

/// **The plugin gate, settled fail-closed.** Under any plugin but the builtin no partition is
/// composed at all, and containment stays on the masked-count route — the same served rank, at the
/// cost the partition exists to remove. The expression is over term signatures, which is sound
/// only where an entity's visibility is decided by its own term set; a foreign plugin's rule
/// cannot be shown to be, and nothing in the corpus forbids one that is not.
#[test]
fn no_partition_is_built_under_a_plugin_that_is_not_the_builtin() {
    let fx = build_fixture();
    let builtin = tessera_plugin::Plugin::data_plugin_hash(&tessera_plugin::Passthrough::new());
    assert!(signature_shaped(&builtin));
    assert!(!signature_shaped("a plugin nobody here has seen"));

    let projections = ArtifactProjections::new(std::env::temp_dir());
    let foreign = PartitionSource {
        postings: &fx.postings,
        data_plugin_hash: "a plugin nobody here has seen",
    };
    let (rows, _) = projections.get_or_build(
        "v00000",
        "s0",
        LAYER,
        0,
        &fx.store,
        &fx.row_space,
        Some(&foreign),
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert!(
        rows.partition().is_none(),
        "a foreign plugin must not get a partition over term signatures"
    );
    assert_eq!(
        projections.partitions(),
        0,
        "and the gauge says so, which is what makes the declining visible to an operator"
    );

    // The same level under the builtin does get one, so the case above is the gate and not an
    // accident of the fixture.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    let native = PartitionSource {
        postings: &fx.postings,
        data_plugin_hash: &builtin,
    };
    let (rows, _) = projections.get_or_build(
        "v00000",
        "s0",
        LAYER,
        0,
        &fx.store,
        &fx.row_space,
        Some(&native),
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert!(rows.partition().is_some());
    assert_eq!(projections.partitions(), 1);

    // **A second view of the same level composes nothing.** The expression is over term
    // signatures, which no row space is involved in, so the dear half of a level's build — one
    // pass over every term's posting — is paid once however many views carry the layer. What is
    // per view is the projection-loss test, and that stays on the row form.
    let (second, _) = projections.get_or_build(
        "v00000",
        "s1",
        LAYER,
        0,
        &fx.store,
        &fx.row_space,
        Some(&native),
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert!(second.partition().is_some());
    assert_eq!(projections.builds(), 2, "two views, two row forms");
    assert_eq!(
        projections.partitions(),
        1,
        "and one partition between them"
    );

    // And the served rank is the same either way, which is the property that makes the gate a
    // cost decision rather than a disclosure one.
    let plain = fx.rows();
    let granted: Vec<u32> = (0..TERMS).filter(|t| t % 2 == 0).collect();
    let overlay = Overlay::new();
    let (mask, satisfied, denied) = fx.mask(&granted, &overlay);
    let answers = rows.partition().unwrap().answers(&satisfied);
    for ordinal in (0..fx.artifacts).filter(|o| !fx.reaches_flushed(*o)) {
        assert_eq!(
            plain.satisfied_rank(ordinal, &mask, true),
            rows.satisfied_rank_via(ordinal, &answers, &denied, true)
                .unwrap(),
            "the gate changed an answer at ordinal {ordinal}"
        );
    }
}

/// **The adoption rule, both directions.** A fold-written partition is taken only where the level
/// it seeded is at exactly the version the file was composed at — and dropped, so the level
/// recomposes, at any other version.
///
/// The direction that matters is the second. A level moves by publication and by growth, and both
/// only *add*: a growth adds members to a generating set, which makes containment **harder**, so a
/// reader that adopted a partition composed before it would answer the easier question. That is
/// the permissive direction on the one test **I3** exists to make conservative, which is why the
/// rule is equality and why nothing weaker is acceptable — a `>=`, a "close enough", or a check
/// only on the layer's name would each admit exactly that.
#[test]
fn a_partition_is_adopted_at_its_own_coordinate_and_at_no_other() {
    let fx = build_fixture();
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("partitions/default/containment")).unwrap();
    let rel = "partitions/default/containment/containment-000001-000.tscp";
    std::fs::write(tmp.path().join(rel), fx.partition().as_bytes()).unwrap();

    // A coordinate that is a number rather than a zero, so *moved down* is expressible as well as
    // *moved up*: a level's version is a counter of writes, and a reader must not treat "lower"
    // as "older and therefore safe".
    let mut store = fx.store.clone();
    store.seed_level_version(LAYER, 0, 7);
    let composed_at = store.level_version(LAYER, 0);
    assert_eq!(composed_at, 7);
    let entry = |version: u64| tessera_store::manifest::DerivedExtent {
        path: rel.to_string(),
        layer: LAYER.to_string(),
        level: 0,
        level_version: version,
        view: None,
        incarnation: None,
        form: tessera_store::manifest::DerivedForm::Containment,
    };
    let source = PartitionSource {
        postings: &fx.postings,
        data_plugin_hash: &tessera_plugin::Plugin::data_plugin_hash(
            &tessera_plugin::Passthrough::new(),
        ),
    };

    // The coordinate holds: mapped, and the level's first request composes nothing.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_all(tmp.path(), "v00000", &[entry(composed_at)], &store);
    assert_eq!(projections.adopted(), 1);
    let (rows, _) = projections.get_or_build(
        "v00000",
        "s0",
        LAYER,
        0,
        &store,
        &fx.row_space,
        Some(&source),
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert!(rows.partition().is_some());
    assert_eq!(
        projections.partitions(),
        0,
        "the adopted partition answered, so nothing was composed"
    );

    // The level has moved since: dropped, and the level recomposes on first use.
    for moved in [composed_at + 1, composed_at.saturating_sub(1)] {
        let projections = ArtifactProjections::new(std::env::temp_dir());
        projections.adopt_all(tmp.path(), "v00000", &[entry(moved)], &store);
        assert_eq!(
            projections.adopted(),
            0,
            "a partition composed at {moved} must not answer for a level at {composed_at}"
        );
        let (rows, _) = projections.get_or_build(
            "v00000",
            "s0",
            LAYER,
            0,
            &store,
            &fx.row_space,
            Some(&source),
            tessera_types::layer::ServingLayout::ArtifactMajor,
            None,
            0,
            false,
        );
        assert!(rows.partition().is_some());
        assert_eq!(projections.partitions(), 1, "the level recomposed instead");
    }

    // And a file the manifest names that is not there is an absence, not a refusal to open: the
    // level recomposes, which is what every request did before the fold wrote anything.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    let mut missing = entry(composed_at);
    missing.path = "partitions/default/containment/gone.tscp".to_string();
    projections.adopt_all(tmp.path(), "v00000", &[missing], &store);
    assert_eq!(projections.adopted(), 0);
}

/// A prefix other than the one being served does not answer, whatever the coordinate says. Row
/// space renumbers wholesale at a fold, and a partition is composed against a prefix's postings —
/// so a partition held for one prefix must not be reused under another.
#[test]
fn an_adopted_partition_does_not_answer_under_another_prefix() {
    let fx = build_fixture();
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("partitions/default/containment")).unwrap();
    let rel = "partitions/default/containment/p.tscp";
    std::fs::write(tmp.path().join(rel), fx.partition().as_bytes()).unwrap();

    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_all(
        tmp.path(),
        "v00000",
        &[tessera_store::manifest::DerivedExtent {
            path: rel.to_string(),
            layer: LAYER.to_string(),
            level: 0,
            level_version: fx.store.level_version(LAYER, 0),
            view: None,
            incarnation: None,
            form: tessera_store::manifest::DerivedForm::Containment,
        }],
        &fx.store,
    );
    assert_eq!(projections.adopted(), 1);
    let source = PartitionSource {
        postings: &fx.postings,
        data_plugin_hash: &tessera_plugin::Plugin::data_plugin_hash(
            &tessera_plugin::Passthrough::new(),
        ),
    };
    let _ = projections.get_or_build(
        "v00001",
        "s0",
        LAYER,
        0,
        &fx.store,
        &fx.row_space,
        Some(&source),
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert_eq!(
        projections.partitions(),
        1,
        "the adopted partition belongs to v00000 and must not answer for v00001"
    );
}

/// The composition reads the postings and nothing else, so the expression it interns is the
/// entity's **own** term set — checked against the fixture's generator for a handful of ordinals,
/// because a signature inversion that silently returned the empty set would make every expression
/// unsatisfiable and every one of the agreement cases above pass by both arms saying *no*.
#[test]
fn the_composed_expression_is_the_members_own_signatures() {
    let fx = build_fixture();
    let partition = fx.partition();
    let partitioned = fx.rows().with_partition(Some(partition));
    let partition = partitioned.partition().unwrap();

    for ordinal in [0u32, 1, 17, 200] {
        assert!(
            !fx.reaches_flushed(ordinal),
            "ordinal {ordinal} reaches above the base rows, where no expression is composed"
        );
        for (rank, set) in fx.published[ordinal as usize].iter().enumerate() {
            // The one principal that holds exactly the terms this set's members carry, and no
            // others: it must be contained where the set projected whole.
            let granted: Vec<u32> = set
                .iter()
                .filter(|e| **e < UNIVERSE)
                .flat_map(|e| fx.terms_of[*e as usize].clone())
                .collect();
            let satisfied: FxHashSet<TermId> = granted.iter().map(|t| TermId::new(*t)).collect();
            let answers = partition.answers(&satisfied);
            let lossy = set.iter().any(|e| *e >= UNIVERSE);
            assert_eq!(
                answers.satisfies(ordinal, rank),
                Some(true),
                "the expression at ({ordinal}, {rank}) is not the members' own signatures"
            );
            // Dropping one term the set depends on breaks it, which is what makes the assertion
            // above a statement about *which* terms rather than about how many.
            let entity = *set.iter().find(|e| **e < UNIVERSE).unwrap() as usize;
            let dropped: FxHashSet<TermId> = satisfied
                .iter()
                .copied()
                .filter(|t| !fx.terms_of[entity].contains(&t.raw()))
                .collect();
            let answers = partition.answers(&dropped);
            assert_eq!(answers.satisfies(ordinal, rank), Some(false));

            // **And where the set is lossy the expression still says yes.** That is the whole
            // reason projection loss is checked on the row form rather than folded into the
            // terms: the members' signatures are satisfied and the artifact is still not
            // contained, because one member has no row in this view at all.
            if lossy {
                let (_mask, satisfied, denied) = fx.mask(&granted, &Overlay::new());
                let answers = partition.answers(&satisfied);
                assert_eq!(answers.satisfies(ordinal, rank), Some(true));
                assert_ne!(
                    partitioned.satisfied_rank_via(ordinal, &answers, &denied, true),
                    Some(Containment::Satisfied(rank as u32)),
                    "a set that lost a member in projection was served on a satisfied expression"
                );
            }
        }
    }
}
