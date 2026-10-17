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
use mosaica_engine::{AcceptError, Engine};
use mosaica_lifecycle::membership::IncomingContent;
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_lifecycle::{ExecError, IncomingArtifact, IncomingGrowth};
use mosaica_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
    SuppliedContent, SuppliedRequirement,
};
use mosaica_types::EntityId;

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
        artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
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

/// A source id the subset principal can see, and one it cannot — the fixture gives
/// [`SUBSET_TERM`] to every third source id (`common::terms_of`).
fn seen_by_subset(source_id: u64) -> bool {
    terms_of(source_id).contains(&SUBSET_TERM)
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
) -> mosaica_engine::GrownMembership {
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

/// **A key repeated in one batch with a rank is refused; repeated with members alone it is a join
/// twice.**
///
/// A page is prepared against the set the store holds, and the store applies the pages of one
/// record in order — so two rows against one artifact would both be read against the state before
/// the batch. That is not only a stale count. A page that empties a set withdraws its content and
/// moves the ranks above it down, so the second row's rank then names a content the caller did not
/// write; where the two sets are the same size the cardinality check agrees by coincidence, and
/// one content goes on serving against its old set while another acquires a member nobody declared
/// for it. The batch is refused instead, and the caller sends two requests.
#[test]
fn a_key_repeated_with_a_rank_is_refused_and_repeated_with_members_alone_is_not() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_layer(LAYER)).unwrap();
    // Two contents, the second's set the same size as the first's, which is what makes the
    // rank-shift undetectable by cardinality alone.
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                fx.members(0..300),
                vec![
                    IncomingContent::new(vec!["rank zero".to_string()], fx.members([0, 1])),
                    IncomingContent::new(vec!["rank one".to_string()], fx.members([2, 3])),
                ],
            )],
        )
        .unwrap();
    tick(&engine);

    // The benign repeat: one key twice, members alone, no rank. Both rows join.
    let grown = engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![
                IncomingGrowth::from_entities("t0".into(), fx.members([300])),
                IncomingGrowth::from_entities("t0".into(), fx.members([301])),
            ],
        )
        .expect("a key repeated with members alone is a join twice");
    assert_eq!((grown[0].joined, grown[1].joined), (1, 1));
    tick(&engine);
    assert_eq!(
        artifacts_of(&engine, &full_coverage_credential())[0].masked_count,
        302,
        "both rows joined"
    );

    // The rank-shift case: the first row empties rank 0 and the second names rank 1, which the
    // withdrawal would have moved.
    let refused = engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![
                IncomingGrowth::page_of_entities(
                    "t0".into(),
                    Some(0),
                    fx.members([]),
                    fx.members([0, 1]),
                ),
                IncomingGrowth::page_of_entities(
                    "t0".into(),
                    Some(1),
                    fx.members([4]),
                    fx.members([]),
                ),
            ],
        )
        .expect_err("a key repeated with a rank is refused");
    let text = refused.to_string();
    assert!(text.contains("more than once"), "{text}");
    assert!(text.contains("rank"), "the refusal names what makes it one: {text}");

    tick(&engine);
    assert_eq!(
        label_for(&engine, &full_coverage_credential()).as_deref(),
        Some("rank zero"),
        "and the batch had no effect: rank 0 is where it was, holding the set it was published \
         with, rather than serving against a set the second row moved under it"
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

/// A content a page withdrew stays withdrawn across a restart, and the content ranked below it is
/// served with its own value: each credential is served `wide`, gated by `wide`'s set, before and
/// after the restart, and never `near`, whose set the page emptied.
#[test]
fn the_content_below_a_withdrawn_one_keeps_its_value_across_a_restart() {
    let fx = fixture();
    let served = |engine: &Engine| -> Vec<Option<String>> {
        [full_coverage_credential(), subset_credential()]
            .iter()
            .map(|credential| label_for(engine, credential))
            .collect()
    };
    let expected = vec![Some("wide".to_string()); 2];
    {
        let engine = fx.open();
        engine.register_layer(label_layer(LAYER)).unwrap();
        engine
            .publish_artifacts(
                LAYER.into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("t0".into()),
                    fx.members(0..300),
                    vec![
                        IncomingContent::new(vec!["near".to_string()], fx.members([0, 1])),
                        IncomingContent::new(
                            vec!["wide".to_string()],
                            fx.members((0..90).filter(|s| seen_by_subset(*s))),
                        ),
                    ],
                )],
            )
            .unwrap();
        tick(&engine);
        assert_eq!(page(&engine, &fx, [], [0, 1]).withdrawn, Some(0));
        tick(&engine);
        assert_eq!(served(&engine), expected, "after the page");
    }
    let engine = fx.open();
    assert_eq!(served(&engine), expected, "across a restart");
}

/// Publish `t0` with `near`, generated from a document the subset principal can see, ranked above
/// `wide`, generated from documents it cannot, and tick so both are served.
fn publish_near_above_wide(engine: &Engine, fx: &Fixture) {
    assert!(seen_by_subset(0) && !seen_by_subset(1));
    engine.register_layer(label_layer(LAYER)).unwrap();
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                fx.members(0..300),
                vec![
                    IncomingContent::new(vec!["near".to_string()], fx.members([0])),
                    IncomingContent::new(vec!["wide".to_string()], fx.members(0..90)),
                ],
            )],
        )
        .unwrap();
    tick(engine);
}

/// Between a page that withdraws `near` and the tick that publishes it, the served form still
/// ranks `near` first while `wide` now stands first in the store. The subset principal, who holds
/// `near`'s set and not `wide`'s, is never served `wide` on any route that reads content, and a
/// point names `t0`, and a `member_of` filter resolves it, for each principal exactly where the
/// artifact is served; nobody is served `near` once the page is accepted. `near_set` is `near`'s
/// generating set.
fn withdrawing_near_never_serves_wide_to_the_subset(engine: &Engine, near_set: Vec<EntityId>) {
    let id = artifacts_of(engine, &full_coverage_credential())
        .into_iter()
        .find(|a| a.key.as_deref() == Some("t0"))
        .expect("t0 is served")
        .mosaica_id;
    // Per route, what this principal is served for `t0`: the viewport's artifacts, the artifact
    // read by its identifier, whether a point of the viewport names it, and whether a `member_of`
    // filter on it matches anything.
    let routes = |credential: &[u8]| -> (Option<String>, Option<String>, bool, bool) {
        let session = engine.authorise(credential).unwrap();
        let single = engine
            .artifact(&session, id, "s0", None)
            .unwrap()
            .map(|a| a.content.first().cloned().unwrap_or_default());
        let tagged = engine
            .viewport(
                &session,
                mosaica_engine::viewport::ViewportRequest::new(
                    "s0",
                    0,
                    WHOLE_MAP,
                    N_ITEMS as usize,
                )
                .layers(mosaica_engine::LayerSelection::All),
            )
            .expect("a viewport")
            .points
            .membership
            .iter()
            .flat_map(|column| &column.ids)
            .any(|tag| *tag == Some(id.raw()));
        let member_of =
            mosaica_engine::filter::FilterExpr::MemberOf(mosaica_engine::filter::MemberOfLeaf {
                layer: LAYER.to_string(),
                artifact: id,
            });
        let matched = engine
            .viewport(
                &session,
                mosaica_engine::viewport::ViewportRequest::new(
                    "s0",
                    0,
                    WHOLE_MAP,
                    N_ITEMS as usize,
                )
                .filter(member_of),
            )
            .expect("a viewport")
            .tiles
            .iter()
            .any(|tile| tile.matched > 0);
        (label_for(engine, credential), single, tagged, matched)
    };
    let near = Some("near".to_string());
    let wide = Some("wide".to_string());
    assert_eq!(
        routes(&subset_credential()),
        (near.clone(), near.clone(), true, true)
    );
    assert_eq!(
        routes(&full_coverage_credential()),
        (near.clone(), near, true, true)
    );

    let withdrawing = IncomingGrowth::page_of_entities("t0".into(), Some(0), Vec::new(), near_set);
    let receipt = engine
        .grow_memberships(LAYER.into(), 0, vec![withdrawing])
        .expect("a page that empties `near`'s set");
    assert_eq!(receipt[0].withdrawn, Some(0));
    for (credential, holds_wides_set) in [
        (subset_credential(), false),
        (full_coverage_credential(), true),
    ] {
        let (listed, single, tagged, matched) = routes(&credential);
        for served in [&listed, &single] {
            assert!(
                served.is_none() || (holds_wides_set && *served == wide),
                "served {served:?} before the tick, which is no content whose set this principal \
                 holds"
            );
        }
        assert_eq!(listed, single, "the two routes serve one answer");
        assert_eq!(
            tagged,
            listed.is_some(),
            "a point names t0 exactly where it is served"
        );
        assert_eq!(
            matched,
            listed.is_some(),
            "a member_of filter resolves t0 exactly where it is served"
        );
    }

    tick(engine);
    assert_eq!(routes(&subset_credential()), (None, None, false, false));
    assert_eq!(
        routes(&full_coverage_credential()),
        (wide.clone(), wide, true, true)
    );
}

/// **A verdict and the value it serves are of one version**, while the store holds the
/// publication's values.
#[test]
fn a_page_that_withdraws_a_content_never_serves_the_next_ones_value_before_the_tick() {
    let fx = fixture();
    let engine = fx.open();
    publish_near_above_wide(&engine, &fx);
    withdrawing_near_never_serves_wide_to_the_subset(&engine, fx.members([0]));
}

/// The same, once a restart has restored the record from the prefix, so its values are read from
/// the layer space's content row by position.
#[test]
fn a_page_that_withdraws_a_restored_content_never_serves_the_next_ones_value_before_the_tick() {
    let fx = fixture();
    let near_set = fx.members([0]);
    {
        let engine = fx.open();
        publish_near_above_wide(&engine, &fx);
        // Releases the log, so the next open restores the record from the prefix alone.
        fold(&engine);
    }
    let engine = fx.open();
    withdrawing_near_never_serves_wide_to_the_subset(&engine, near_set);
}

// ---- ranks are positions -----------------------------------------------------------------------

/// Publish `t0` with three contents, `near`, `wide` and `far` in that order, each generated from
/// the sources `sets` gives it, and tick so they are served.
fn publish_three(engine: &Engine, fx: &Fixture, sets: [&[u64]; 3]) {
    engine.register_layer(label_layer(LAYER)).unwrap();
    let contents = ["near", "wide", "far"]
        .into_iter()
        .zip(sets)
        .map(|(value, set)| {
            IncomingContent::new(vec![value.to_string()], fx.members(set.iter().copied()))
        })
        .collect();
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                fx.members(0..300),
                contents,
            )],
        )
        .unwrap();
    tick(engine);
}

/// **A page still in the log when a compaction withdraws a lower rank replays onto the content it
/// named.** `wide` is generated from two documents the subset principal cannot see, and `far` from
/// one it can and one of those two. A page takes that shared document out of `wide`. The
/// compaction then withdraws `near`, whose one document was deleted, and the service stops before
/// the log is rotated, so the restart replays the page over the compacted record.
///
/// What each principal is owed: the subset principal holds neither `wide`'s set nor `far`'s and is
/// served nothing; the full principal is served `wide`. A page replayed one rank down would take
/// the shared document out of `far` instead, with the same cardinality, and serve `far` to the
/// subset principal, who cannot see a document it was generated from.
#[test]
fn a_page_replayed_after_a_compaction_withdrew_a_lower_rank_moves_the_same_set() {
    let fx = fixture();
    let (deleted, visible, hidden, shared) = (6, 3, 2, 1);
    assert!(seen_by_subset(visible) && !seen_by_subset(hidden) && !seen_by_subset(shared));
    let served = |engine: &Engine| -> Vec<Option<String>> {
        [full_coverage_credential(), subset_credential()]
            .iter()
            .map(|credential| label_for(engine, credential))
            .collect()
    };
    let owed = vec![Some("wide".to_string()), None];
    let log = {
        let engine = fx.open();
        publish_three(
            &engine,
            &fx,
            [&[deleted], &[hidden, shared], &[visible, shared]],
        );
        // The publication leaves the log, so the restart reads the record from the prefix alone.
        reclaim_log(&engine);
        let receipt = engine
            .grow_memberships(
                LAYER.into(),
                0,
                vec![IncomingGrowth::page_of_entities(
                    "t0".into(),
                    Some(1),
                    Vec::new(),
                    fx.members([shared]),
                )],
            )
            .expect("a page of `wide`'s set");
        assert_eq!((receipt[0].joined, receipt[0].left), (0, 1));
        engine
            .accept_change(fx.members([deleted])[0], ChangeOp::Delete)
            .expect("the delete is accepted");
        tick(&engine);
        assert_eq!(served(&engine), owed, "after the page");
        let log = snapshot_wal(fx._tmp.path());
        fold(&engine);
        assert_eq!(
            engine.last_fold_report()[0].contents_lost,
            vec![(0, 1)],
            "the compaction withdrew `near`"
        );
        assert_eq!(served(&engine), owed, "after the compaction");
        log
    };
    // A crash between the compaction's publication and the log's rotation.
    restore_wal(fx._tmp.path(), &log);
    let engine = fx.open();
    assert_eq!(
        served(&engine),
        owed,
        "across a restart that replays the page"
    );
}

/// **A fill after a withdrawal takes the next position, and a fill at a withdrawn rank is a
/// conflict.** On a layer whose content serves on the artifact's own gate, `x` and `y` are
/// withdrawn by pages that join a document to each set and take it out again; a content filled at
/// rank 0 is refused as the conflict a differing part is, one at rank 3 names a rank past the
/// next, and one at rank 2 is taken and served, before and after a restart.
#[test]
fn a_fill_after_a_withdrawal_takes_the_next_position() {
    let fx = fixture();
    let fill = |engine: &Engine, rank: u16| {
        let mut growth = IncomingGrowth::from_entities("t0".into(), Vec::new());
        growth.parts.contents = vec![(rank, vec!["z".to_string()])];
        engine.grow_memberships(LAYER.into(), 0, vec![growth])
    };
    {
        let engine = fx.open();
        let mut layer = label_layer(LAYER);
        layer.content.supplied[0].require_member_visibility = SuppliedRequirement::Inherited;
        engine.register_layer(layer).unwrap();
        let contents = ["x", "y"]
            .map(|value| IncomingContent::new(vec![value.to_string()], Vec::new()))
            .into();
        engine
            .publish_artifacts(
                LAYER.into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("t0".into()),
                    fx.members(0..300),
                    contents,
                )],
            )
            .unwrap();
        for rank in [0, 1] {
            let paged = |joining, leaving| {
                engine
                    .grow_memberships(
                        LAYER.into(),
                        0,
                        vec![IncomingGrowth::page_of_entities(
                            "t0".into(),
                            Some(rank),
                            joining,
                            leaving,
                        )],
                    )
                    .expect("a page")
                    .remove(0)
            };
            paged(fx.members([0]), Vec::new());
            assert_eq!(paged(Vec::new(), fx.members([0])).withdrawn, Some(rank));
        }

        assert!(matches!(
            fill(&engine, 0),
            Err(AcceptError::Exec(ExecError::PartConflict { .. }))
        ));
        assert!(matches!(
            fill(&engine, 3),
            Err(AcceptError::Exec(ExecError::LayerRefused { .. }))
        ));
        assert_eq!(fill(&engine, 2).expect("the next position")[0].filled, 1);
        tick(&engine);
        assert_eq!(
            label_for(&engine, &subset_credential()).as_deref(),
            Some("z")
        );
    }
    let engine = fx.open();
    assert_eq!(
        label_for(&engine, &subset_credential()).as_deref(),
        Some("z"),
        "across a restart"
    );
}
