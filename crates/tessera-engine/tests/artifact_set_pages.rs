//! **A generating set is the caller's claim, changed by pages, and served as the tick published
//! it.**
//!
//! Decision 0135 makes what a content was derived from the caller's own claim, and `ingest.md`
//! §1.1 gives it one mechanism: a page at the content's rank, naming the entities joining and the
//! entities leaving, with the set's stored cardinality moved by the same page. Three properties
//! decide whether that is safe, and each has a case here.
//!
//! **The pair.** Containment is `|G ∩ M| == |G|`, so it reads two numbers that must come from one
//! version of the set: the row-space operator and the cardinality it was derived with. They are
//! published together at the flush tick and the store's own cardinality never reaches a request
//! before then, so a request between a page and the tick sees the old pair whole rather than half
//! of each.
//!
//! **The leave.** A union cannot express a leave. An operator unioned forward while the
//! cardinality moved down would pass containment for a principal who holds the member that left
//! and none of the rest — content generated from documents they cannot see, served to them. So a
//! delta holding a leave re-derives that operator from entity truth, and the case below is written
//! from the principal who would have been served.
//!
//! **The floor.** An empty set is contained in every mask, so a content on one serves to every
//! principal who reaches the artifact (decision 0107). A page that empties a set therefore
//! withdraws the content, says so in its acknowledgement, and does not bring it back when the set
//! refills.

mod common;

use common::*;
use tessera_engine::{ArtifactOut, Engine, ViewportRequest};
use tessera_lifecycle::membership::IncomingContent;
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
    SuppliedContent, SuppliedRequirement,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const LAYER: &str = "topics/a";

/// A label layer whose content is served only to a viewer who can see every document it was
/// generated from — the declaration that makes containment the question.
fn label_layer(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "label".into(),
                ty: "text".into(),
                require_member_visibility: SuppliedRequirement::All,
            }],
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        open_engine_publishing(&self.root, &self.cache, &self.wal)
    }

    fn members(&self, source_ids: impl IntoIterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids
            .into_iter()
            .map(|s| EntityId::new(map[&s]))
            .collect()
    }
}

/// A source id the subset principal can see, and one it cannot — the fixture gives
/// [`SUBSET_TERM`] to every third source id (`common::terms_of`).
fn seen_by_subset(source_id: u64) -> bool {
    terms_of(source_id).contains(&SUBSET_TERM)
}

fn artifacts_of(engine: &Engine, credential: &[u8]) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// The content this principal is served for `t0`, or `None` where the artifact is withheld.
fn label_for(engine: &Engine, credential: &[u8]) -> Option<String> {
    artifacts_of(engine, credential)
        .into_iter()
        .find(|a| a.key.as_deref() == Some("t0"))
        .map(|a| a.content.first().cloned().unwrap_or_default())
}

/// Publish `t0` with one content generated from `generated_from`, and tick so it is served.
fn publish(engine: &Engine, fx: &Fixture, generated_from: impl IntoIterator<Item = u64>) {
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                fx.members(0..300),
                vec![IncomingContent::new(
                    vec!["a label".to_string()],
                    fx.members(generated_from),
                )],
            )],
        )
        .expect("a publication carrying a content and its set");
    tick(engine);
}

/// One page at rank 0: entities joining the generating set and entities leaving it.
fn page(
    engine: &Engine,
    fx: &Fixture,
    joining: impl IntoIterator<Item = u64>,
    leaving: impl IntoIterator<Item = u64>,
) -> tessera_engine::GrownMembership {
    engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![IncomingGrowth::page_of_entities(
                "t0".into(),
                Some(0),
                fx.members(joining),
                fx.members(leaving),
            )],
        )
        .expect("a page of a generating set")
        .remove(0)
}

// ---- the delta -------------------------------------------------------------------------------

/// **Joins are applied before leaves inside one page** (`ingest.md` §1.1), which is what makes a
/// paged replacement safe: every intermediate set is a superset of the old and the new, so the
/// content is never served to a principal who satisfies neither.
///
/// The page names one entity in both lists. Applied joins-first it leaves the set, and the
/// resulting cardinality is the one the acknowledgement reports; applied the other way round it
/// would stay in.
#[test]
fn joins_are_applied_before_leaves_within_one_page() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();
    publish(&engine, &fx, [0, 1]);

    // 2 joins and leaves in the same page; 1 only leaves. Joins first: {0,1,2} then minus {1,2}.
    let receipt = page(&engine, &fx, [2], [1, 2]);
    assert_eq!(
        (receipt.joined, receipt.left),
        (1, 2),
        "the page joined one entity the set did not hold and took two out of it"
    );
    tick(&engine);

    // The broad principal is served whatever the set is; the subset principal is the reader that
    // says which set it is. Source id 0 carries the subset term and 1 and 2 do not, so the set
    // {0} is satisfied and any set holding 1 or 2 is not.
    assert!(seen_by_subset(0) && !seen_by_subset(1) && !seen_by_subset(2));
    assert_eq!(
        label_for(&engine, &subset_credential()).as_deref(),
        Some("a label"),
        "the set is {{0}}: 2 joined and then left with 1, which is joins before leaves"
    );
}

/// **A membership never shrinks** (`ingest.md` §10, R7). The two removal rules of write-path §5.4
/// govern how a member leaves one, and a page that could take members out would be a third route.
/// The refusal names the rank a caller who meant a generating set wanted.
#[test]
fn a_page_that_leaves_a_membership_is_refused() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();
    publish(&engine, &fx, [0, 1]);

    let refused = engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![IncomingGrowth::page_of_entities(
                "t0".into(),
                None,
                fx.members([]),
                fx.members([0]),
            )],
        )
        .expect_err("a membership never shrinks");
    let text = refused.to_string();
    assert!(text.contains("never shrinks"), "{text}");
    assert!(text.contains("rank"), "the refusal names the spelling that does: {text}");

    tick(&engine);
    assert_eq!(
        artifacts_of(&engine, &full_coverage_credential())[0].masked_count,
        300,
        "and nothing left the membership"
    );
}

// ---- the pair --------------------------------------------------------------------------------

/// **The cardinality moves with the page, and a request reads the pair the tick published.**
///
/// The page joins a document the subset principal cannot see. Between the acknowledgement and the
/// tick the request is served against the operator *and* the cardinality of the smaller set — the
/// pair as last published — and never the new cardinality against the old operator, which is a
/// test nobody made. After the tick both have moved and the principal fails.
#[test]
fn the_cardinality_and_the_operator_move_together_at_the_tick() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();
    // Source id 0 carries the subset term, so the set {0} is satisfied by both principals.
    publish(&engine, &fx, [0]);
    assert_eq!(
        label_for(&engine, &subset_credential()).as_deref(),
        Some("a label")
    );

    // Source id 1 does not carry it, so the set {0, 1} is satisfied by the broad principal alone.
    assert!(!seen_by_subset(1));
    let receipt = page(&engine, &fx, [1], []);
    assert_eq!((receipt.joined, receipt.left), (1, 0));

    assert_eq!(
        label_for(&engine, &subset_credential()).as_deref(),
        Some("a label"),
        "before the tick the request is served the pair as last published: the set of one, with \
         the cardinality it was derived with"
    );

    tick(&engine);
    assert_eq!(
        label_for(&engine, &subset_credential()),
        None,
        "and after it the pair the page moved: a set of two, one of which this principal cannot \
         see"
    );
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("a label"),
        "the principal who can see both is served throughout"
    );
}

/// **A page holding a leave re-derives the operator from entity truth, and the principal who holds
/// the member that left is not served.**
///
/// This is the false pass the design names (`ingest.md` §1.1). The set is `{visible, hidden}`,
/// where the subset principal can see the first and not the second. The page takes the *visible*
/// one out, so the declared set is `{hidden}` and that principal satisfies nothing. An operator
/// unioned forward would still hold both members while the cardinality had moved to one, and
/// `|G ∩ M| == |G|` would then read `1 == 1` and serve them content generated from a document
/// they may not see.
#[test]
fn a_leave_re_derives_the_operator_and_the_holder_of_the_left_member_is_not_served() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();

    let visible = (0..300).find(|s| seen_by_subset(*s)).expect("one visible");
    let hidden = (0..300).find(|s| !seen_by_subset(*s)).expect("one hidden");
    publish(&engine, &fx, [visible, hidden]);
    assert_eq!(
        label_for(&engine, &subset_credential()),
        None,
        "the set holds a document this principal cannot see, so it is not served"
    );

    let receipt = page(&engine, &fx, [], [visible]);
    assert_eq!((receipt.joined, receipt.left), (0, 1));
    tick(&engine);

    assert_eq!(
        label_for(&engine, &subset_credential()),
        None,
        "the declared set is now the hidden document alone. A principal holding the member that \
         left and none of the rest must not be served: an operator carried forward by a union \
         would hold both members against a cardinality of one, and the intersection over the one \
         member they hold would read as containment"
    );
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("a label"),
        "and the principal who can see the whole declared set still is"
    );
}

// ---- the floor -------------------------------------------------------------------------------

/// **A page that empties a set withdraws the content, reports it, and refilling the set does not
/// bring it back** (`ingest.md` §1.1; decision 0107's rule at the caller's door).
///
/// An empty set is contained in every mask, so a content retained on one would serve to every
/// principal reaching the artifact. The record is removed rather than retained and not served,
/// which is why the rank that held it names no content afterwards.
#[test]
fn a_page_that_empties_a_set_withdraws_the_content_and_a_refill_does_not_restore_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();
    publish(&engine, &fx, [0, 1]);
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("a label")
    );

    let receipt = page(&engine, &fx, [], [0, 1]);
    assert_eq!(
        receipt.withdrawn,
        Some(0),
        "the acknowledgement names the rank the page emptied, beside the key the caller sent"
    );
    tick(&engine);
    assert_eq!(
        label_for(&engine, &full_coverage_credential()),
        None,
        "the layer declares supplied content and this artifact has none, so it is withheld whole \
         rather than served with its description missing (decision 0076)"
    );

    let refused = engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![IncomingGrowth::page_of_entities(
                "t0".into(),
                Some(0),
                fx.members([0, 1]),
                fx.members([]),
            )],
        )
        .expect_err("the content is gone, so there is no set at that rank to refill");
    assert!(refused.to_string().contains("no content at rank 0"), "{refused}");
    tick(&engine);
    assert_eq!(
        label_for(&engine, &full_coverage_credential()),
        None,
        "and the content did not come back: the caller supplies it again"
    );
}

// ---- the tick --------------------------------------------------------------------------------

/// **A level under continuous paging is served from the last published form, and no request builds
/// one** (`ingest.md` §1.3, §10 ruling 6).
///
/// Twelve pages arrive with a request between each, and the build counter does not move: every one
/// of those requests is answered from the form the last tick published. The answers are stale by
/// up to a tick and understate — a member not yet in an operator is not counted — which is the
/// direction the design allows.
#[test]
fn a_level_under_continuous_paging_never_builds_a_form_on_a_request() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();
    publish(&engine, &fx, [0]);
    // Warm: whatever the first request derives, it derives before the pages.
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("a label")
    );
    let warm = engine.artifact_cache_builds().0;

    for source in 1..13u64 {
        page(&engine, &fx, [source], []);
        assert_eq!(
            label_for(&engine, &full_coverage_credential()).as_deref(),
            Some("a label"),
            "served from the form the last tick published"
        );
    }
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "no request projected the level: the pages reach the form at the tick and a request is \
         served whatever was last published"
    );

    tick(&engine);
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("a label"),
        "and the tick published all twelve pages onto the form it already held"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "the tick amended the held form rather than building one"
    );
    assert_eq!(
        label_for(&engine, &subset_credential()),
        None,
        "the twelve pages joined documents the subset principal cannot see"
    );
}

/// **The pages replay**: the set and its stored cardinality come back from the log, and the
/// artifact serves what it served before the restart.
#[test]
fn a_set_page_replays_from_the_log() {
    let fx = fixture();
    let visible: Vec<u64> = (0..300).filter(|s| seen_by_subset(*s)).take(2).collect();
    let hidden = (0..300).find(|s| !seen_by_subset(*s)).expect("one hidden");
    {
        let engine = fx.open();
        engine.register_layer(label_layer(LAYER)).unwrap();
        publish(&engine, &fx, [visible[0], hidden]);
        // Out with the document the subset principal cannot see, in with one it can.
        page(&engine, &fx, [visible[1]], [hidden]);
        tick(&engine);
        assert_eq!(
            label_for(&engine, &subset_credential()).as_deref(),
            Some("a label"),
            "the repaired set is satisfied"
        );
    }

    let engine = fx.open();
    assert_eq!(
        label_for(&engine, &subset_credential()).as_deref(),
        Some("a label"),
        "the replayed page left the same set behind, with the cardinality it moved: a set restored \
         without its page would hold the hidden document and be satisfied by nobody"
    );
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("a label")
    );
}
