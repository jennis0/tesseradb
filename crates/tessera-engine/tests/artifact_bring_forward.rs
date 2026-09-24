//! **A write amends the level's row form; it does not invalidate it.**
//!
//! A growth, a publication and a flush each change what a level's row form should say, and each of
//! them used to change it by moving a coordinate the form was filed under — so the next request
//! naming that level projected the whole thing again. At rung 3 that was 94 to 177 s inside a
//! request against a 60 s stream deadline
//! (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`). The deltas are small and they
//! are known where they are accepted, so they are applied there.
//!
//! **The file's spine is the last test.** Amending a held form and building one from scratch are
//! two routes to one structure, and a maintained form that is *close* is a masked count that is
//! wrong with nothing anywhere to notice — which is the failure mode the whole artifact stage is
//! written against. So `an_amended_form_equals_one_built_from_scratch` compares the two ordinal for
//! ordinal, bitmap for bitmap, extent for extent and, on the row-major route, count for count. The
//! cases above it say which writes are covered and what a viewer sees; that one says the answer is
//! the same object.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ServingLayout,
};
use tessera_types::EntityId;

const LAYER: &str = "clusters/a";
const LABELS: &str = "topics/a";

/// **No existence criterion**, as in the fold's and the growth's own cases: these assertions are
/// about what a count *is*, and a criterion would turn a wrong count into an absence, which is the
/// weaker of the two observations.
fn declaration(name: &str, layout: Option<ServingLayout>) -> LayerDeclaration {
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
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout,
        shape: None,
    }
}

/// The same layer declaring one supplied content, so its artifacts carry generating sets and a
/// page at a rank has something to move.
fn label_declaration(name: &str) -> LayerDeclaration {
    let mut declaration = declaration(name, None);
    declaration.content = ContentDeclaration {
        computed: Vec::new(),
        supplied: vec![tessera_types::layer::SuppliedContent {
            name: "label".into(),
            ty: "text".into(),
            require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
        }],
    };
    declaration
}

/// The same layer with an **open** value set, so a batch naming a key no artifact holds mints one.
fn open_declaration(name: &str, layout: Option<ServingLayout>) -> LayerDeclaration {
    LayerDeclaration {
        value_set: tessera_types::layer::ValueSet::Open,
        ..declaration(name, layout)
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The corpus entities behind a run of source ids.
    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }

    /// The corpus entities behind a list of source ids, for the sets a page names.
    fn ids(&self, source_ids: &[u64]) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.iter().map(|s| EntityId::new(map[s])).collect()
    }
}

/// Every served artifact's key and masked count, ascending by key — what a viewer is told, which
/// is what every assertion here is finally about.
fn served(engine: &Engine) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = artifacts_of(engine, &full_coverage_credential())
        .into_iter()
        .map(|a| (a.key.unwrap_or_default(), a.masked_count))
        .collect();
    out.sort();
    out
}

/// A publication and the tick that publishes its delta into the level's row forms.
///
/// **Two moments** (`ingest.md` §1.3): the acknowledgement means durable, and the write reaches
/// what a viewer is served at the next publication. Every assertion here is about what is served,
/// so the tick belongs in the helper rather than in each test.
fn publish(engine: &Engine, key: &str, members: Vec<EntityId>) {
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::from_entities(Some(key.into()), members)],
        )
        .expect("a publication into a registered layer");
    tick(engine);
}

fn grow(engine: &Engine, key: &str, members: Vec<EntityId>) {
    engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![IncomingGrowth::from_entities(key.into(), members)],
        )
        .expect("points joining an artifact that exists is an ordinary write");
    tick(engine);
}

/// Publish one artifact carrying a content per generating set given, and tick.
fn publish_labelled(
    engine: &Engine,
    fx: &Fixture,
    key: &str,
    members: Vec<EntityId>,
    sets: &[Vec<u64>],
) {
    let contents: Vec<tessera_lifecycle::membership::IncomingContent> = sets
        .iter()
        .enumerate()
        .map(|(rank, sources)| {
            tessera_lifecycle::membership::IncomingContent::new(
                vec![format!("{key} label {rank}")],
                fx.ids(sources),
            )
        })
        .collect();
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some(key.into()),
                members,
                contents,
            )],
        )
        .expect("a publication carrying contents and their sets");
    tick(engine);
}

/// One page of one content's generating set, and the tick that publishes it.
fn page(engine: &Engine, fx: &Fixture, key: &str, rank: u16, joining: Vec<u64>, leaving: Vec<u64>) {
    engine
        .grow_memberships(
            LABELS.into(),
            0,
            vec![IncomingGrowth::page_of_entities(
                key.into(),
                Some(rank),
                fx.ids(&joining),
                fx.ids(&leaving),
            )],
        )
        .expect("a page of a generating set");
    tick(engine);
}

/// One ingested point, under a batch key used once — a second ingest under a key already seen is
/// *replayed* rather than accepted, so a fixed key would silently ingest nothing the second time.
fn ingest(engine: &Engine, external_id: &[u8]) -> EntityId {
    let descriptors = vec![b"0".to_vec()];
    let mut key = [0u8; 32];
    for (slot, byte) in key.iter_mut().zip(external_id) {
        *slot = *byte;
    }
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(external_id.to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(
            vec![row],
            String::from_utf8_lossy(external_id).into_owned(),
            key,
        )
        .expect("the ingest is accepted")[0]
}

/// One ingest batch of `names.len()` points, each naming one artifact of `LAYER` by key — a
/// **single commit window**, which is what puts a mint and a growth into one apply.
fn ingest_naming(engine: &Engine, batch: &str, names: &[&str]) -> u64 {
    let descriptors = vec![b"0".to_vec()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(batch.as_bytes()) {
        *slot = *byte;
    }
    let rows: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, _)| tessera_lifecycle::command::UnallocatedRow {
            external_id: Some(format!("{batch}-{i}").into_bytes()),
            view: "s0".to_string(),
            join: None,
            descriptors: descriptors.clone(),
            x: 5.0,
            y: 5.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&descriptors),
            scoped: Vec::new(),
        })
        .collect();
    let memberships = names
        .iter()
        .enumerate()
        .map(|(i, key)| tessera_lifecycle::BatchMembership {
            layer: LAYER.to_string(),
            level: 0,
            view: None,
            key: (*key).to_string(),
            rows: vec![i as u32],
        })
        .collect();
    let (_, minted) = engine
        .accept_ingest_joining(
            rows,
            batch.to_string(),
            hash,
            tessera_lifecycle::BatchArtifacts {
                memberships,
                edges: Vec::new(),
            },
        )
        .expect("points naming artifacts of an open layer are an ordinary write");
    tick(engine);
    minted
}

/// Flush enough times to fill the merge policy's tier, then let one merge publish — the one
/// operation that renumbers rows a form now holds. Returns the points it ingested, one per
/// flushed segment, whose rows the merge renumbers.
fn merge(engine: &Engine) -> Vec<EntityId> {
    let merges = engine.write_executor_stats().merges;
    let mut ingested = Vec::new();
    engine.set_merge_for_test(false);
    for batch in [
        b"merge-a".as_slice(),
        b"merge-b".as_slice(),
        b"merge-c".as_slice(),
        b"merge-d".as_slice(),
    ] {
        ingested.push(ingest(engine, batch));
        flush(engine);
    }
    engine.set_merge_for_test(true);
    ingested.push(ingest(engine, b"merge-e"));
    flush(engine);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().merges == merges {
        assert!(
            std::time::Instant::now() < deadline,
            "the merge never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ingested
}

// ---- (a) the delta reaches the held form ---------------------------------------------------

/// **A growth is served from the form that was already warm.**
///
/// The count moves and the build counter does not: the level's version moved, the held form was
/// amended with the entities that joined, and its key moved with it. A form left to be rebuilt
/// would answer the same and cost the level's whole projection to do it.
#[test]
fn a_growth_is_applied_to_the_warm_form_rather_than_rebuilding_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();
    publish(&engine, "a0", fx.members(0..100));
    publish(&engine, "a1", fx.members(500..600));

    // Warm: whatever the first request derives, it derives before the growth.
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 100)]
    );
    let warm = engine.artifact_cache_builds().0;

    grow(&engine, "a0", fx.members(100..150));

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 150), ("a1".to_string(), 100)],
        "the members that joined are in the very next request's count, and the artifact that did \
         not grow is where it was"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "nothing was projected again: the delta the write accepted is the delta the form took"
    );
}

/// **A publication into a warm level is the same story one ordinal along.** The new artifact serves
/// from the form that was already held, which the level's version move would otherwise have
/// discarded whole.
#[test]
fn a_publication_is_applied_to_the_warm_form_rather_than_rebuilding_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();
    publish(&engine, "a0", fx.members(0..100));
    assert_eq!(served(&engine), vec![("a0".to_string(), 100)]);
    let warm = engine.artifact_cache_builds().0;

    publish(&engine, "a1", fx.members(500..600));

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 100)],
        "the artifact published second is served on the next request, at its own count"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "the new ordinal was placed in the form that was held, not projected beside a new one"
    );
}

/// Every row's label list, read off the level's held column — the observable a column *is*.
fn labels(engine: &Engine, layer: &str) -> Vec<Vec<u32>> {
    let rows = engine
        .held_artifact_form_for_test("s0", layer, 0)
        .expect("the level's form is held");
    let column = rows.column().expect("this level is served row-major");
    (0..column.row_count())
        .map(|row| {
            let mut at = Vec::new();
            column.for_each_label(row, |ordinal| at.push(ordinal));
            at.sort_unstable();
            at
        })
        .collect()
}

/// The bytes of the level's packed column — the durable half, which an amendment may not touch.
fn column_bytes(engine: &Engine, layer: &str) -> Vec<u8> {
    engine
        .held_artifact_form_for_test("s0", layer, 0)
        .expect("the level's form is held")
        .column()
        .expect("this level is served row-major")
        .as_bytes()
        .to_vec()
}

/// **A growth amends the column at the rows that joined and nowhere else.**
///
/// The pack is not rewritten — its bytes are the same bytes — and the labels differ from the
/// pre-growth ones at exactly the joined rows. Composing the column again would answer the same
/// and cost one entry per membership entry: ~100 s at rung 3's `mesh/descriptors`, on the executor
/// thread, for one entity joining three artifacts.
#[test]
fn a_growth_amends_the_column_at_the_joined_rows_and_composes_nothing() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(LAYER, Some(ServingLayout::RowMajorList)))
        .unwrap();
    publish(&engine, "a0", fx.members(0..100));
    publish(&engine, "a1", fx.members(500..600));
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 100)]
    );

    let before_labels = labels(&engine, LAYER);
    let before_bytes = column_bytes(&engine, LAYER);
    let composed = engine.columns_composed();

    // The joiners are members of `a1` already, so the rows they hold are rows the column labels —
    // an amendment in the middle of the pack, not an append above it.
    grow(&engine, "a0", fx.members(500..520));

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 120), ("a1".to_string(), 100)],
        "the growth is in the very next request"
    );
    assert_eq!(
        engine.columns_composed(),
        composed,
        "no column was composed: the amendment is the delta, not a second walk of the level"
    );
    assert_eq!(
        column_bytes(&engine, LAYER),
        before_bytes,
        "the packed column is shared, not rewritten"
    );

    let after_labels = labels(&engine, LAYER);
    assert_eq!(
        after_labels.len(),
        before_labels.len(),
        "the amendment named no row above the column's own"
    );
    let joined: std::collections::BTreeSet<usize> = after_labels
        .iter()
        .zip(&before_labels)
        .enumerate()
        .filter(|(_, (after, before))| after != before)
        .map(|(row, _)| row)
        .collect();
    assert_eq!(
        joined.len(),
        20,
        "exactly the twenty rows that joined moved: {joined:?}"
    );
    for row in &joined {
        let mut gained: Vec<u32> = after_labels[*row].clone();
        gained.retain(|ordinal| !before_labels[*row].contains(ordinal));
        assert_eq!(
            gained,
            vec![0],
            "row {row} gained the growing artifact's ordinal and nothing else"
        );
    }
}

/// **One window that mints an artifact and grows another on the same level rebuilds nothing.**
///
/// Both records move that level's version, and both are applied to the store before either reaches
/// a row form — so a form stamped at the store's *current* version would be filed at the version of
/// two deltas while holding one, and the second delta would then find a mismatch and discard the
/// form it was about to complete. Each record bumps its level exactly once, so each amendment
/// stamps the version it followed plus one.
#[test]
fn a_window_that_mints_and_grows_one_level_rebuilds_nothing() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(open_declaration(LAYER, None))
        .unwrap();
    publish(&engine, "a0", fx.members(0..100));
    assert_eq!(served(&engine), vec![("a0".to_string(), 100)]);
    let warm = engine.artifact_cache_builds().0;

    // One batch, two rows: the first names the artifact that exists (a growth), the second a key
    // no artifact holds (a mint, which publishes). One window, two records, one level.
    assert_eq!(
        ingest_naming(&engine, "mixed", &["a0", "made-by-a-point"]),
        1,
        "one key named no artifact, so exactly one was minted"
    );

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100)],
        "both rows are still buffered, so neither is in a count and the minted artifact — whose          only member is one of them — is in no viewport"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "neither record's delta discarded the other's form"
    );

    // And the rows the window ingested reach both artifacts at their flush, still without a
    // rebuild: the mint's delta and the growth's are both in the form the flush then extends.
    flush(&engine);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101), ("made-by-a-point".to_string(), 1)],
        "the grown artifact gained its row and the minted one is served on its own"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "and the flush extended the form rather than replacing it"
    );
}

// ---- (c) the flush extends the form by the segment it publishes -----------------------------

/// **An ingested member counts from its flush, before any fold.**
///
/// The member has no row while it is buffered and an *extent* row once the flush publishes; the
/// form covers the whole row space, and the flush extends every held form by the segment it wrote.
/// The fold that follows renumbers the row and changes no count, which is the second half of the
/// same assertion: what the fold does here is move rows, not find members.
#[test]
fn an_ingested_member_counts_at_its_flush_and_the_fold_changes_nothing() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();

    // The tick that publishes the artifact also flushes the buffered row, so the flush is held
    // until the buffered count has been read.
    engine.set_flush_paused_for_test(true);
    let fresh = ingest(&engine, b"joins-a0");
    let mut members = fx.members(0..100);
    members.push(fresh);
    publish(&engine, "a0", members);

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100)],
        "buffered: the member has no row anywhere, so it is in no count"
    );

    let before = engine.write_executor_stats().flushes;
    engine.set_flush_paused_for_test(false);
    tick_until(
        &engine,
        "the held flush to publish",
        std::time::Duration::from_secs(60),
        || engine.write_executor_stats().flushes > before,
    );
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101)],
        "flushed: it holds an extent row and the flush put that segment's rows into the form"
    );

    fold(&engine);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101)],
        "folded: the same member on a base row — the fold renumbers what is counted, not how many"
    );
}

/// The same for a member that joins by **growth** after its own flush: it holds an extent row
/// already, so what has to reach the form is the join rather than the segment.
#[test]
fn a_member_that_joins_after_its_flush_counts_at_the_growth() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();
    publish(&engine, "a0", fx.members(0..100));

    let fresh = ingest(&engine, b"joins-later");
    flush(&engine);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100)],
        "the flushed point is in no artifact yet"
    );

    grow(&engine, "a0", vec![fresh]);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101)],
        "an extent row joins a membership exactly as a base row does"
    );
}

/// **A growth into rows an earlier growth gave another artifact falls back on the label form.**
///
/// The first growth labels rows the pack left unclaimed, so the second claim is in the amendment
/// rather than in the pack. A label column cannot carry two artifacts at one row, so the level is
/// served artifact-major from that write on, and the counts a viewer is told are unchanged.
#[test]
fn a_growth_into_rows_already_amended_falls_back_on_the_label_form() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(LAYER, Some(ServingLayout::RowMajorLabel)))
        .unwrap();
    publish(&engine, "a0", fx.members(0..100));
    publish(&engine, "a1", fx.members(500..600));
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 100)]
    );
    let fallbacks = engine.layout_fallbacks();

    grow(&engine, "a1", fx.members(700..720));
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 120)]
    );
    assert_eq!(engine.layout_fallbacks(), fallbacks, "disjoint rows still partition");
    assert!(
        engine
            .held_artifact_form_for_test("s0", LAYER, 0)
            .expect("held")
            .column()
            .is_some(),
        "the label column took the first growth"
    );

    grow(&engine, "a0", fx.members(700..710));
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 110), ("a1".to_string(), 120)],
        "the answers are unchanged by the layout"
    );
    assert_eq!(
        engine.layout_fallbacks(),
        fallbacks + 1,
        "a row that would carry two artifacts is refused by the label form"
    );
    assert!(
        engine
            .held_artifact_form_for_test("s0", LAYER, 0)
            .expect("held")
            .column()
            .is_none(),
        "the level is served artifact-major from that write on"
    );
}

// ---- the merge rebases the form in place -----------------------------------------------------

/// **A merge is taken by the warm form, not paid by the next request.**
///
/// A row-space merge renumbers the rows inside the span it collapses, which are rows the form
/// holds. The publication rebases the form over the merged extent before the swap; nothing is
/// projected on the request that follows, and the counts on the other side are the memberships'
/// own sizes. Before the rebase the form failed `covers` and the level was projected whole inside
/// that request: 108 s at rung 3, shed (`probes/2026-09-05-merge-arm/`).
///
/// Both layouts, because only one of them holds a column to rebase.
#[test]
fn a_merge_is_applied_to_the_warm_form_rather_than_rebuilding_it() {
    for layout in [ServingLayout::ArtifactMajor, ServingLayout::RowMajorList] {
        let fx = fixture();
        let engine = fx.open();
        engine
            .register_layer(declaration(LAYER, Some(layout)))
            .unwrap();
        publish(&engine, "a0", fx.members(0..100));
        publish(&engine, "a1", fx.members(500..600));
        // A member on an extent row before the merge, so the span the merge renumbers holds a
        // labelled row and not only unclaimed ones.
        let fresh = ingest(&engine, b"pre-merge");
        flush(&engine);
        grow(&engine, "a0", vec![fresh]);
        assert_eq!(
            served(&engine),
            vec![("a0".to_string(), 101), ("a1".to_string(), 100)]
        );
        let warm = engine.artifact_cache_builds().0;
        let composed = engine.columns_composed();

        let merged = merge(&engine);

        assert_eq!(
            served(&engine),
            vec![("a0".to_string(), 101), ("a1".to_string(), 100)],
            "{layout:?}: the merge renumbered rows this form holds and changed no count"
        );
        assert_eq!(
            engine.artifact_cache_builds().0,
            warm,
            "{layout:?}: nothing was projected again: the form was rebased at the publication"
        );
        assert_eq!(
            engine.columns_composed(),
            composed,
            "{layout:?}: no column was composed: the amendment gave the span up and took it back"
        );
        // And the rebased form is the form: a member on a merged row joins at its new row.
        grow(&engine, "a1", vec![merged[2]]);
        assert_eq!(
            served(&engine),
            vec![("a0".to_string(), 101), ("a1".to_string(), 101)],
            "{layout:?}: a row the merge renumbered joins an artifact at the row it now holds"
        );
        assert_eq!(engine.artifact_cache_builds().0, warm);
    }
}

// ---- the differential ------------------------------------------------------------------------

/// Every observable half of one level's row form, in a shape two forms can be compared on.
fn form_of(engine: &Engine, layer: &str) -> Vec<String> {
    let rows = engine
        .held_artifact_form_for_test("s0", layer, 0)
        .expect("the level's form is held");
    let mut out = vec![format!(
        "ordinals={} layout={:?} row_count={} everywhere={}",
        rows.len(),
        rows.layout(),
        rows.index().row_count(),
        rows.index().everywhere()
    )];
    for ordinal in 0..rows.len() as u32 {
        let membership = rows
            .get(ordinal)
            .map(|bitmap| bitmap.to_vec())
            .map(|v| format!("{v:?}"))
            .unwrap_or_else(|| "hole".to_string());
        let generating: Vec<String> = rows
            .membership()
            .generating(ordinal)
            .iter()
            .map(|set| format!("{:?}", set.to_vec()))
            .collect();
        // The row-major route's own count of an artifact's rows, which is what a column answers
        // from and the artifact-major route never reads.
        let declared = rows
            .column()
            .map(|column| column.declared_size(ordinal).to_string())
            .unwrap_or_else(|| "-".to_string());
        // **The pair, not the operator alone** (`ingest.md` §1.1): a containment test reads each
        // generating set beside the cardinality it was derived with, so a differential that
        // compared only the sets would pass a form whose cardinalities came from another version.
        let sizes = rows.records().declared_sizes(ordinal);
        out.push(format!(
            "{ordinal}: rows={membership} extent={:?} generating={generating:?} sizes={sizes:?} \
             declared={declared}",
            rows.index().extent(ordinal)
        ));
    }
    out
}

/// **The maintained form is the built form.**
///
/// A publication, a growth, a flush and a growth over the flushed row all amend one held form in
/// place; the same sequence is then compared against the form built from scratch over the same
/// records and the same row space. Equal ordinal for ordinal, row bitmap for row bitmap, extent
/// for extent, generating set for generating set — and on the row-major route the column's own
/// per-artifact count too, which is the number that route serves from.
///
/// Run under both layouts, because they are the two routes a count is reached by and only one of
/// them holds a column at all.
#[test]
fn an_amended_form_equals_one_built_from_scratch() {
    for layout in [ServingLayout::ArtifactMajor, ServingLayout::RowMajorLabel] {
        let fx = fixture();
        let engine = fx.open();
        engine
            .register_layer(declaration(LAYER, Some(layout)))
            .unwrap();

        // Warm the form, then move it by every route that moves it.
        publish(&engine, "a0", fx.members(0..100));
        let warm = served(&engine);
        assert_eq!(warm, vec![("a0".to_string(), 100)]);

        publish(&engine, "a1", fx.members(500..600));
        grow(&engine, "a0", fx.members(100..150));
        let fresh = ingest(&engine, b"differential");
        flush(&engine);
        grow(&engine, "a1", vec![fresh]);
        // **A publication *after* the flush**, so the ordinal it places is placed into a form whose
        // row space carries an extent — the case `publish_at` takes and a pre-flush publication
        // does not reach.
        let later = ingest(&engine, b"differential-later");
        flush(&engine);
        publish(&engine, "a2", vec![later]);

        let maintained_answers = served(&engine);
        let maintained = form_of(&engine, LAYER);
        // Both routes have to be *taken*, or the row-major run asserts the artifact-major one
        // twice: a level whose memberships turned out to overlap falls back, correctly and
        // silently, and these two are disjoint on purpose so it does not.
        assert!(
            maintained[0].contains(&format!("layout={layout:?}")),
            "{layout:?}: the amended form fell back to another layout, so this run proves nothing \
             about the one it names — {}",
            maintained[0]
        );
        assert_eq!(
            maintained_answers,
            vec![
                ("a0".to_string(), 150),
                ("a1".to_string(), 101),
                ("a2".to_string(), 1)
            ],
            "{layout:?}: the maintained form's counts are the memberships' own sizes"
        );

        // From scratch: every derived structure for this layer dropped, and the next request
        // projects the level over the same records and the same row space.
        engine.forget_artifact_forms_for_test(LAYER);
        let rebuilt_answers = served(&engine);
        let rebuilt = form_of(&engine, LAYER);

        assert_eq!(
            maintained_answers, rebuilt_answers,
            "{layout:?}: a viewer is told the same thing by the two forms"
        );
        assert_eq!(
            maintained.len(),
            rebuilt.len(),
            "{layout:?}: the two forms cover the same ordinals"
        );
        for (amended, built) in maintained.iter().zip(&rebuilt) {
            assert_eq!(
                amended, built,
                "{layout:?}: the amended form and the built one describe different levels"
            );
        }

        // **And the one publication that permutes rows the form holds.** A merge collapses a run
        // of extents into one and re-sorts the rows inside it; the publication rebases every held
        // form over the merged extent before the swap. The sequence then continues on the rebased
        // form — a growth naming a row the merge renumbered, and a publication placing an ordinal
        // over the merged row space — and the whole is compared once more with a form built from
        // scratch. Nothing here was projected again: the builds counter is read before the merge
        // and after the publication.
        let builds = engine.artifact_cache_builds().0;
        let merged = merge(&engine);
        // **No second merge**, so that what follows is the writes and nothing else: the ticks that
        // publish them would otherwise take the merge policy's next chance and renumber rows the
        // comparison below holds fixed.
        engine.set_merge_for_test(false);
        let after_merge = form_of(&engine, LAYER);
        assert_eq!(
            served(&engine),
            maintained_answers,
            "{layout:?}: the merge renumbered rows this form holds and changed no count"
        );
        grow(&engine, "a0", vec![merged[0], merged[3]]);
        publish(&engine, "a3", vec![merged[1], merged[4]]);
        let after_writes = served(&engine);
        assert_eq!(
            after_writes,
            vec![
                ("a0".to_string(), 152),
                ("a1".to_string(), 101),
                ("a2".to_string(), 1),
                ("a3".to_string(), 2)
            ],
            "{layout:?}: rows the merge renumbered join and publish at the rows they now hold"
        );
        assert_eq!(
            engine.artifact_cache_builds().0,
            builds,
            "{layout:?}: the merge, the growth after it and the publication after it were all \
             taken by the held form"
        );
        let maintained = form_of(&engine, LAYER);
        assert!(
            maintained[0].contains(&format!("layout={layout:?}")),
            "{layout:?}: the rebased form fell back to another layout — {}",
            maintained[0]
        );

        engine.forget_artifact_forms_for_test(LAYER);
        assert_eq!(
            served(&engine),
            after_writes,
            "{layout:?}: the form built from scratch over the merged row space agrees"
        );
        let rebuilt = form_of(&engine, LAYER);
        assert_eq!(
            maintained.len(),
            rebuilt.len(),
            "{layout:?}: the two forms cover the same ordinals after the merge"
        );
        for (amended, built) in maintained.iter().zip(&rebuilt) {
            assert_eq!(
                amended, built,
                "{layout:?}: the rebased form and the built one describe different levels"
            );
        }
        // The form as it stood between the merge and the writes is the one both routes agree on
        // too: every ordinal below the two writes' is unchanged by them.
        for (ordinal, line) in after_merge.iter().enumerate().skip(1) {
            let touched = line.starts_with("0:") || line.starts_with("3:");
            if !touched {
                assert_eq!(
                    line, &rebuilt[ordinal],
                    "{layout:?}: an ordinal the writes after the merge did not touch differs"
                );
            }
        }
    }
}

/// **A level whose generating sets are paged is maintained into the form a build produces.**
///
/// The differential above moves memberships; this one moves the other set an artifact carries. A
/// page of joins is unioned into the served operator, a page holding a leave re-derives that
/// operator from entity truth, and a page that empties a set withdraws the content and moves every
/// rank above it — three arms, one structure, compared against the level projected from scratch
/// over the same records: operator for operator and cardinality for cardinality
/// (`ingest.md` §1.1, §4.1).
#[test]
fn a_paged_generating_set_is_maintained_into_the_form_a_build_produces() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_declaration(LABELS)).unwrap();

    // Two artifacts, one carrying two ranked contents so a withdrawal has a rank above it to move.
    publish_labelled(
        &engine,
        &fx,
        "t0",
        fx.members(0..100),
        &[(0..40).collect(), (0..80).collect()],
    );
    publish_labelled(
        &engine,
        &fx,
        "t1",
        fx.members(100..200),
        &[(100..140).collect()],
    );
    let warm = served(&engine);
    assert_eq!(
        warm,
        vec![("t0".to_string(), 100), ("t1".to_string(), 100)],
        "both are served before anything is paged"
    );

    // Joins alone: unioned into the operator that is served.
    page(&engine, &fx, "t0", 1, (80..90).collect(), Vec::new());
    // A leave: the operator re-derived whole.
    page(&engine, &fx, "t1", 0, (140..150).collect(), (100..110).collect());
    // A page that empties rank 0 of `t0`, which withdraws it and moves rank 1 down to 0.
    page(&engine, &fx, "t0", 0, Vec::new(), (0..40).collect());

    let maintained_answers = served(&engine);
    let maintained = form_of(&engine, LABELS);

    engine.forget_artifact_forms_for_test(LABELS);
    let rebuilt_answers = served(&engine);
    let rebuilt = form_of(&engine, LABELS);

    assert_eq!(
        maintained_answers, rebuilt_answers,
        "a viewer is told the same thing by the two forms"
    );
    assert_eq!(
        maintained.len(),
        rebuilt.len(),
        "the two forms cover the same ordinals"
    );
    for (amended, built) in maintained.iter().zip(&rebuilt) {
        assert_eq!(
            amended, built,
            "the maintained form and the built one describe different levels"
        );
    }
    assert!(
        maintained[1].contains("sizes=[90]"),
        "t0 kept one content — the rank above the withdrawn one, moved down to rank 0 and holding \
         the eighty it was published with plus the ten its own page joined — {}",
        maintained[1]
    );
}

// ---- a merge under a generating set ----------------------------------------------------------

/// The merge policy's shipped width: how many adjacent same-tier extents select a merge.
const TIER_WIDTH: usize = 4;
/// Points per interleaved segment. Above [`TIER_WIDTH`], so the merged order cycles through every
/// segment more than once and no row's position is a coincidence of the first cycle.
const ROWS_EACH: usize = 8;

/// Flush [`TIER_WIDTH`] segments of [`ROWS_EACH`] points each, laid out so that the segments
/// **interleave in Morton order**, and return the entities by `[segment][t]`.
///
/// Segment `s` takes the x positions congruent to `s` modulo [`TIER_WIDTH`], at one y. Morton
/// order over a constant y is monotone in x, so the merged extent orders the rows
/// `s0t0, s1t0, s2t0, s3t0, s0t1, …`: point `(s, t)` holds row `ROWS_EACH·s + t` inside the span
/// before the merge and `TIER_WIDTH·t + s` after it. A point per flush at ascending x would give
/// extents already in Morton order, which the merged segment concatenates unchanged, and every
/// assertion about a moved row is then true of the identity.
///
/// `descriptors_of` gives each point its access descriptors by `(s, t)`.
fn flush_interleaved(
    engine: &Engine,
    descriptors_of: impl Fn(usize, usize) -> Vec<Vec<u8>>,
) -> Vec<Vec<EntityId>> {
    let mut by_segment = Vec::new();
    for s in 0..TIER_WIDTH {
        let rows: Vec<_> = (0..ROWS_EACH)
            .map(|t| {
                let descriptors = descriptors_of(s, t);
                tessera_lifecycle::command::UnallocatedRow {
                    external_id: Some(format!("interleaved-{s}-{t}").into_bytes()),
                    view: "s0".to_string(),
                    join: None,
                    x: ((t * TIER_WIDTH + s) * 20) as f64,
                    y: 5.0,
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                    scoped: Vec::new(),
                }
            })
            .collect();
        by_segment.push(
            engine
                .accept_ingest(rows, format!("interleaved-{s}"), [s as u8 + 1; 32])
                .expect("the ingest is accepted"),
        );
        flush(engine);
    }
    by_segment
}

/// One entity's row, or `None` while it is buffered.
fn row_of(engine: &Engine, entity: EntityId) -> Option<u32> {
    engine.generation().bundle.partitions["default"].views["s0"]
        .row_space
        .row_of(entity)
        .map(|row| row.raw())
}

/// Let one merge publish over the extents [`flush_interleaved`] wrote, and return each watched
/// entity's row before and after it. Merge is turned off again on the way out, so the ticks that
/// follow do not take the policy's next chance and move the rows an assertion holds fixed.
fn run_merge(engine: &Engine, watched: &[EntityId]) -> (Vec<Option<u32>>, Vec<Option<u32>>) {
    let before: Vec<Option<u32>> = watched.iter().map(|e| row_of(engine, *e)).collect();
    let merges = engine.write_executor_stats().merges;
    engine.set_merge_for_test(true);
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().merges == merges {
        assert!(
            std::time::Instant::now() < deadline,
            "the merge never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    engine.set_merge_for_test(false);
    let after: Vec<Option<u32>> = watched.iter().map(|e| row_of(engine, *e)).collect();
    (before, after)
}

/// Every artifact key this principal is served, ascending. A viewer who contains no content's
/// generating set entirely receives no artifact at all rather than one with the content dropped
/// (`ArtifactOut::content`), so a key absent here is a content withheld.
fn served_to(engine: &Engine, credential: &[u8]) -> Vec<String> {
    let session = engine.authorise(credential).unwrap();
    let mut keys: Vec<String> = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
        .into_iter()
        .filter_map(|a| a.key)
        .collect();
    keys.sort();
    keys
}

/// Publish one artifact of `LABELS` whose one content is generated from `generated_from`, and tick.
fn publish_generated_from(
    engine: &Engine,
    key: &str,
    members: Vec<EntityId>,
    generated_from: Vec<EntityId>,
) {
    engine
        .publish_artifacts(
            LABELS.into(),
            0,
            vec![IncomingArtifact::with_content(
                Some(key.into()),
                members,
                vec![tessera_lifecycle::membership::IncomingContent::new(
                    vec![format!("{key} label 0")],
                    generated_from,
                )],
            )],
        )
        .expect("a publication carrying a content and its set");
    tick(engine);
}

/// **A merge that moves a generating set's member gives the set the row the member now holds.**
///
/// A generating set is a set of *rows*, and a merge renumbers the rows inside the span it
/// collapses. The rebase clears each set over the span and re-projects it from the record's
/// entities, so the set names the same entities afterwards. Carry the rows across instead and the
/// set keeps its cardinality while naming whatever the merge put at those rows — which serves an
/// all-gated content to a principal who holds the new occupant and not the member, and withholds
/// it from the principal who holds the member.
///
/// So this asserts both directions at the serving level, over a span whose merged order is
/// **provably** not the concatenation: the member's row before the merge is the successor's row
/// after it, asserted here rather than assumed of the fixture.
#[test]
fn a_merge_that_moves_a_generating_sets_member_re_projects_the_set() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(label_declaration(LABELS)).unwrap();
    engine.set_merge_for_test(false);

    // The content's one member, and the point that takes its row: `(1, 1)` holds row `8·1 + 1`
    // inside the span before the merge and `4·1 + 1` after it, and `(1, 2)` holds `4·2 + 1`
    // after — the row `(1, 1)` gave up. See [`flush_interleaved`] for the layout.
    const MEMBER: (usize, usize) = (1, 1);
    const SUCCESSOR: (usize, usize) = (1, 2);
    let segments = flush_interleaved(&engine, |s, t| match (s, t) {
        // The member is visible to the broad principal alone, its successor to the narrow
        // principal alone; every other point to both.
        MEMBER => vec![b"0".to_vec()],
        SUCCESSOR => vec![b"1".to_vec()],
        _ => vec![b"0".to_vec(), b"1".to_vec()],
    });
    let member = segments[MEMBER.0][MEMBER.1];
    let successor = segments[SUCCESSOR.0][SUCCESSOR.1];

    // Thirty built members, of which the narrow principal sees ten, so this artifact has a
    // non-zero masked count for both principals and an absence below is the content's gate rather
    // than an empty membership.
    publish_generated_from(&engine, "m0", fx.members(0..30), vec![member]);

    assert_eq!(
        served_to(&engine, &full_coverage_credential()),
        vec!["m0".to_string()],
        "the broad principal holds the set's one member and is served the content"
    );
    assert_eq!(
        served_to(&engine, &subset_credential()),
        Vec::<String>::new(),
        "the narrow principal lacks that member and is served nothing"
    );

    let builds = engine.artifact_cache_builds().0;
    let (before, after) = run_merge(&engine, &[member, successor]);

    assert!(
        before.iter().chain(&after).all(Option::is_some),
        "both watched points must hold a row on each side of the merge: {before:?} -> {after:?}"
    );
    assert_ne!(
        before[0], after[0],
        "the merge left the member where it was, so the rest of this case is about the identity \
         permutation — see flush_interleaved"
    );
    assert_eq!(
        before[0], after[1],
        "the successor must occupy the member's old row, or the two principals below are not \
         separated by the rebase: {before:?} -> {after:?}"
    );

    assert_eq!(
        served_to(&engine, &full_coverage_credential()),
        vec!["m0".to_string()],
        "the set holds the member at the row it now holds, so the principal who holds the member \
         is still served"
    );
    assert_eq!(
        served_to(&engine, &subset_credential()),
        Vec::<String>::new(),
        "the narrow principal holds the point that now occupies the member's old row and not the \
         member itself, and must be served nothing"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        builds,
        "the merge was taken by the held form, so those two answers are the maintained form's \
         rather than a rebuild's"
    );

    // And the maintained form is the built form, generating set for generating set.
    let maintained = form_of(&engine, LABELS);
    engine.forget_artifact_forms_for_test(LABELS);
    assert_eq!(
        served_to(&engine, &full_coverage_credential()),
        vec!["m0".to_string()],
        "the form built from scratch over the merged row space agrees"
    );
    let rebuilt = form_of(&engine, LABELS);
    assert_eq!(
        maintained.len(),
        rebuilt.len(),
        "the two forms cover the same ordinals"
    );
    for (amended, built) in maintained.iter().zip(&rebuilt) {
        assert_eq!(
            amended, built,
            "the rebased form and the built one describe different levels"
        );
    }
}
