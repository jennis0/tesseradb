//! **`POST /v1/artifacts/browse`: a layer's hierarchy by lineage, independent of the viewport**
//! (`highlight-and-hierarchy.md` §4).
//!
//! The oracle is the planted tree itself: every artifact's members are a run of source ids, so
//! which of them a principal sees is `terms_of`'s own arithmetic and the counts are computed here
//! rather than read back from the engine.
//!
//! What is asserted:
//!
//! - **The three forms**: roots, one artifact's children with its own parents beside them, and a
//!   name search — each row carrying the masked count the artifacts frame carries.
//! - **The gate runs before the page.** A criterion the narrow principal fails withholds a node
//!   from every form at once, its children become roots (C29 per entry), and a page's fill and its
//!   `next` count only artifacts that cleared it — so a withheld artifact leaves no gap.
//! - **The order is total and the cursor walks it exactly**: paging one row at a time reproduces
//!   the unpaged answer, over tied counts included.
//! - **The counts under a filter**: `matched_count` is `|membership ∩ M_auth ∩ filter|`, existence
//!   and `masked_count` do not move with it, a row whose `matched_count` is zero is still served,
//!   and the order becomes the filtered one. **A leaf over a render-only column is answered by the
//!   whole-view scan** the owner ruled (§9 (d)) rather than silently zero, which is the failure
//!   decision 0104 exists to prevent.
//! - **The refusals are about deployment schema and never about an artifact**: an unknown layer, a
//!   `level` on a one-level kind and `limit = 0` refuse; a `parent` that names nothing, one of
//!   another layer and one withheld by the criterion answer an empty page.

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, Int32Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::browse::{BrowseForm, BrowseOut, BrowseRequest, BrowseRow};
use tessera_engine::filter::{Endpoint, FilterExpr, FilterOperand, Scalar};
use tessera_engine::{Engine, EngineError};
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, SuppliedContent, SuppliedRequirement,
};
use tessera_types::{EntityId, TesseraId};

const N: u64 = 900;
const LAYER: &str = "clusters/tree";

/// **`score` is `render = true, index = false`** — the shape a viewport answers only inside its
/// tiles, and the one whose browse count is the whole-view scan §9 (d) ruled. `archive` is
/// indexed, so a clause over it routes entity space and is projected whole; having both is what
/// makes the two routes comparable in one fixture.
const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  xx = 11
  yy = 22
  zz = 33

[[attribute]]
name       = "archive"
type       = "category"
render     = false
index      = true
vocabulary = "archive"

[[attribute]]
name     = "score"
type     = "i32"
render   = true
index    = false
"#;

fn archive_of(e: u64) -> &'static str {
    ["xx", "yy", "zz"][(e / 2 % 3) as usize]
}

fn score_of(e: u64) -> i32 {
    (e as i32 * 7 % 101) - 50
}

fn subset_sees(e: u64) -> bool {
    terms_of(e).contains(&SUBSET_TERM)
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("archive", DataType::Utf8, true),
        Field::new("score", DataType::Int32, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let archives: Vec<Option<String>> = ids
        .iter()
        .map(|&e| Some(archive_of(e).to_string()))
        .collect();
    let scores: Vec<i32> = ids.iter().map(|&e| score_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives)),
            Arc::new(Int32Array::from(scores)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities: Vec<u64> = Vec::new();
    let mut terms: Vec<u32> = Vec::new();
    for e in 0..N {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn declaration(criterion: Option<ExistenceCriterion>) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: LAYER.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: criterion,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Nested,
            prune_children: false,
        },
        // A supplied text content, which is what a browse row's `name` is and what the search form
        // reads beside the key.
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![SuppliedContent {
                name: "label".into(),
                ty: "text".into(),
                require_member_visibility: SuppliedRequirement::Inherited,
            }],
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// The planted forest: three roots of 300 members each, each with three children of 100.
///
/// The member runs are contiguous in source id, so every count in this file is `terms_of`'s own
/// arithmetic over a range and nothing is read back from the engine to check the engine.
const ROOTS: [(&str, u64); 3] = [("alpha", 0), ("bravo", 300), ("charlie", 600)];

fn children_of(root: &str) -> [(String, u64); 3] {
    let base = ROOTS
        .iter()
        .find(|(key, _)| *key == root)
        .expect("a planted root")
        .1;
    [
        (format!("{root}-one"), base),
        (format!("{root}-two"), base + 100),
        (format!("{root}-three"), base + 200),
    ]
}

struct Fixture {
    _dir: tempfile::TempDir,
    engine: Engine,
    /// The vocabulary's key-to-code bindings, read from the built manifest: a category leaf names
    /// a code, and reading the declaration back would be the test agreeing with itself.
    codes: HashMap<String, u32>,
}

fn fixture(criterion: Option<ExistenceCriterion>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    let bundle = dir.path().join("bundle");
    let schema_path = dir.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &HashMap::new()).unwrap().schema;
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &schema),
        out: bundle.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the fixture builds");
    let engine = open_engine_publishing(
        &bundle,
        &dir.path().join("cache"),
        &dir.path().join("wal.log"),
    );
    engine.register_layer(declaration(criterion)).unwrap();
    let map = source_to_new_map(&bundle, "v00000");
    let members = |range: std::ops::Range<u64>| -> Vec<EntityId> {
        range.map(|s| EntityId::new(map[&s])).collect()
    };
    let mut planted = Vec::new();
    for (root, base) in ROOTS {
        let mut node = IncomingArtifact::from_entities(Some(root.into()), members(base..base + 300));
        node.contents = vec![tessera_lifecycle::membership::IncomingContent {
            values: vec![format!("The {root} group")],
            generated_from: Default::default(),
        }];
        planted.push(node);
        for (key, at) in children_of(root) {
            let mut child =
                IncomingArtifact::from_entities(Some(key.clone()), members(at..at + 100));
            child.parent_keys = vec![root.to_string()];
            child.contents = vec![tessera_lifecycle::membership::IncomingContent {
                values: vec![format!("Subgroup {key}")],
                generated_from: Default::default(),
            }];
            planted.push(child);
        }
    }
    engine
        .publish_artifacts(LAYER.into(), 0, planted)
        .unwrap();
    let opened = tessera_store::read::open_bundle(&bundle).expect("the fixture opens");
    let codes = opened
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "archive")
        .expect("the manifest records the vocabulary")
        .values
        .iter()
        .map(|v| (v.key.clone(), v.code))
        .collect();
    Fixture {
        _dir: dir,
        engine,
        codes,
    }
}

/// The page's keys, sorted — the *set* a form serves, compared apart from the order, which
/// [`is_in_total_order`] checks on its own terms.
fn key_set(out: &BrowseOut) -> Vec<String> {
    let mut keys = keys(out);
    keys.sort();
    keys
}

/// **The total order, checked as a property rather than as a transcript.** Count descending, then
/// `tessera_id` ascending. The identifiers are a blinding permutation (I10), so an alphabetical
/// expectation would be asserting the permutation rather than the rule.
fn is_in_total_order(out: &BrowseOut) -> bool {
    out.artifacts.windows(2).all(|w| {
        let count = |r: &BrowseRow| r.matched_count.unwrap_or(r.masked_count);
        (std::cmp::Reverse(count(&w[0])), w[0].tessera_id.raw())
            < (std::cmp::Reverse(count(&w[1])), w[1].tessera_id.raw())
    })
}

/// The oracle: how many of `range` this principal sees.
fn visible_in(range: std::ops::Range<u64>, broad: bool) -> u64 {
    range.filter(|&e| broad || subset_sees(e)).count() as u64
}

fn browse(
    engine: &Engine,
    credential: &[u8],
    form: BrowseForm,
    filter: Option<FilterExpr>,
    limit: usize,
) -> BrowseOut {
    let session = engine.authorise(credential).unwrap();
    engine
        .browse(
            &session,
            BrowseRequest {
                view: "s0",
                layer: LAYER,
                level: None,
                form,
                filter,
                limit,
                cursor: None,
            },
        )
        .expect("a browse answers")
}

fn keys(out: &BrowseOut) -> Vec<String> {
    out.artifacts
        .iter()
        .map(|row| row.key.clone().unwrap_or_default())
        .collect()
}

fn id_of(fx: &Fixture, credential: &[u8], key: &str) -> TesseraId {
    let out = browse(&fx.engine, credential, BrowseForm::Roots, None, 100);
    if let Some(row) = out.artifacts.iter().find(|r| r.key.as_deref() == Some(key)) {
        return row.tessera_id;
    }
    for root in out.artifacts {
        let children = browse(
            &fx.engine,
            credential,
            BrowseForm::Children(root.tessera_id),
            None,
            100,
        );
        if let Some(row) = children
            .artifacts
            .iter()
            .find(|r| r.key.as_deref() == Some(key))
        {
            return row.tessera_id;
        }
    }
    panic!("'{key}' is not served to this principal");
}

/// **The three forms, and every count is the oracle's.**
///
/// Roots are the artifacts with no served parent; children are the artifacts naming one; a search
/// matches on the key and on the supplied name alike. `parents` is present on the children form
/// and empty on the others.
#[test]
fn the_three_forms_serve_the_lineage_and_the_oracles_counts() {
    let fx = fixture(None);
    for credential in [full_coverage_credential(), subset_credential()] {
        let broad = credential == full_coverage_credential();
        let roots = browse(&fx.engine, &credential, BrowseForm::Roots, None, 100);
        assert_eq!(key_set(&roots), vec!["alpha", "bravo", "charlie"]);
        assert!(is_in_total_order(&roots), "count descending, identifier ascending");
        assert!(roots.parents.is_empty(), "`parents` is the children form's");
        assert!(roots.next.is_none(), "three rows fit one page");
        for row in &roots.artifacts {
            let key = row.key.clone().expect("a planted key");
            let base = ROOTS
                .iter()
                .find(|(k, _)| *k == key)
                .expect("a planted root")
                .1;
            assert_eq!(row.name.as_deref(), Some(format!("The {key} group").as_str()));
            assert_eq!(row.masked_count, visible_in(base..base + 300, broad));
            assert_eq!(row.matched_count, None, "no filter, no question");
            assert_eq!(row.rung, 0, "a root is at depth 0");
            assert!(row.parent_ids.is_empty());
        }

        let alpha = roots
            .artifacts
            .iter()
            .find(|r| r.key.as_deref() == Some("alpha"))
            .expect("alpha is served")
            .tessera_id;
        let children = browse(
            &fx.engine,
            &credential,
            BrowseForm::Children(alpha),
            None,
            100,
        );
        assert_eq!(
            key_set(&children),
            vec!["alpha-one", "alpha-three", "alpha-two"]
        );
        assert!(
            is_in_total_order(&children),
            "tied counts break by identifier, and the order is total"
        );
        assert!(
            children.parents.is_empty(),
            "a root has no parent to name, and C29 makes that one shape"
        );
        for row in &children.artifacts {
            assert_eq!(row.parent_ids, vec![alpha], "the served parent, named");
            assert_eq!(row.rung, 1, "a child is one deep in the served forest");
        }
        // And the child's own parents come back beside its children — here it has none.
        let leaf = browse(
            &fx.engine,
            &credential,
            BrowseForm::Children(children.artifacts[0].tessera_id),
            None,
            100,
        );
        assert!(leaf.artifacts.is_empty(), "a leaf has no children");
        assert_eq!(
            leaf.parents.iter().map(|r| r.tessera_id).collect::<Vec<_>>(),
            vec![alpha],
            "and its own parent is named beside them"
        );

        // Search matches the key and the supplied name alike, case-insensitively.
        let by_key = browse(
            &fx.engine,
            &credential,
            BrowseForm::Search("BRAVO".into()),
            None,
            100,
        );
        assert_eq!(
            key_set(&by_key),
            vec!["bravo", "bravo-one", "bravo-three", "bravo-two"]
        );
        let by_name = browse(
            &fx.engine,
            &credential,
            BrowseForm::Search("subgroup charlie".into()),
            None,
            100,
        );
        assert_eq!(
            key_set(&by_name),
            vec!["charlie-one", "charlie-three", "charlie-two"],
            "the search reads the supplied name too"
        );
    }
}

/// **The gate runs before the page, and a withheld artifact leaves no gap.**
///
/// The criterion here is one the broad principal clears at every node and the narrow one clears
/// only at the roots: a root's 300 members leave 100 visible, a child's 100 leave 33. So the
/// narrow principal is served the roots and none of the children — the children's absence is the
/// *whole* answer, not a short page — and every count they do see is their own.
#[test]
fn the_criterion_decides_the_page_before_the_limit_does() {
    let fx = fixture(Some(ExistenceCriterion::Count(50)));
    let broad_roots = browse(
        &fx.engine,
        &full_coverage_credential(),
        BrowseForm::Roots,
        None,
        100,
    );
    assert_eq!(broad_roots.artifacts.len(), 3);
    let narrow_roots = browse(
        &fx.engine,
        &subset_credential(),
        BrowseForm::Roots,
        None,
        100,
    );
    assert_eq!(
        key_set(&narrow_roots),
        vec!["alpha", "bravo", "charlie"],
        "the roots clear the bar for the narrow principal too"
    );
    let alpha = narrow_roots.artifacts[0].tessera_id;
    let narrow_children = browse(
        &fx.engine,
        &subset_credential(),
        BrowseForm::Children(alpha),
        None,
        100,
    );
    assert!(
        narrow_children.artifacts.is_empty() && narrow_children.next.is_none(),
        "every child is below this principal's own criterion, so the page is empty and complete"
    );
    let broad_children = browse(
        &fx.engine,
        &full_coverage_credential(),
        BrowseForm::Children(broad_roots.artifacts[0].tessera_id),
        None,
        100,
    );
    assert_eq!(broad_children.artifacts.len(), 3, "and served to the broad one");
}

/// **A child whose parent is withheld is a root here** — C29 per entry, one verb over.
///
/// The criterion is set so a *root* fails for the narrow principal while its children pass: the
/// children then have no served parent, so they are roots of this principal's own forest and their
/// `parent_ids` are empty. Distinguishing them from real roots would disclose that a coarser
/// grouping exists which they are not cleared to see.
#[test]
fn a_child_whose_parent_is_withheld_is_a_root() {
    // A bar every child clears for the narrow principal (33 visible of 100) and every root fails
    // — which needs the roots to be *suppressed* rather than under-count, since a root contains
    // its children. Suppression is the same withholding by a different door, and it is the door a
    // test can drive.
    let fx = fixture(None);
    let idset = fx.engine.generation().bundle.manifest.identity.idset;
    let alpha = id_of(&fx, &full_coverage_credential(), "alpha");
    let entity = fx.engine.resolve_tessera_ids(&[alpha], idset).unwrap()[0].unwrap();
    fx.engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .unwrap();
    let roots = browse(
        &fx.engine,
        &full_coverage_credential(),
        BrowseForm::Roots,
        None,
        100,
    );
    let served: Vec<String> = keys(&roots);
    assert!(
        !served.contains(&"alpha".to_string()),
        "the suppressed root is gone: {served:?}"
    );
    for key in ["alpha-one", "alpha-two", "alpha-three"] {
        let row = roots
            .artifacts
            .iter()
            .find(|r| r.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("{key} is a root now: {served:?}"));
        assert!(
            row.parent_ids.is_empty(),
            "and it names no parent, which is C29's one shape"
        );
        assert_eq!(row.rung, 0, "with nothing above it, it is at depth 0");
    }
}

/// **The order is total and the cursor walks it exactly.**
///
/// Paging one row at a time must reproduce the unpaged answer — over tied counts included, which
/// is what the identifier tie-break is for: a cursor over a partial order would duplicate a row or
/// drop one, and the fixture's nine children all have the same count.
#[test]
fn a_cursor_over_tied_counts_neither_duplicates_nor_drops() {
    let fx = fixture(None);
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let all = browse(
        &fx.engine,
        &full_coverage_credential(),
        BrowseForm::Search("-".into()),
        None,
        100,
    );
    assert_eq!(all.artifacts.len(), 9, "nine children, every count tied");
    assert!(all.next.is_none());
    let mut walked: Vec<BrowseRow> = Vec::new();
    let mut cursor = None;
    loop {
        let page = fx
            .engine
            .browse(
                &session,
                BrowseRequest {
                    view: "s0",
                    layer: LAYER,
                    level: None,
                    form: BrowseForm::Search("-".into()),
                    filter: None,
                    limit: 1,
                    cursor,
                },
            )
            .unwrap();
        assert!(page.artifacts.len() <= 1);
        walked.extend(page.artifacts);
        match page.next {
            None => break,
            Some(next) => cursor = Some(next),
        }
    }
    assert_eq!(walked, all.artifacts, "the walk is the unpaged answer");
}

/// **The counts under a filter**, on both routes.
///
/// `archive` is indexed, so its clause routes entity space and is projected whole; `score` is
/// **render-only**, so its clause is the whole-view scan §9 (d) ruled — served naively it would be
/// silently zero, which is exactly the failure decision 0104 exists to prevent. Both must equal
/// `|membership ∩ M_auth ∩ filter|` computed here from the generator.
///
/// And the three anchoring rules: existence does not move with the filter, `masked_count` does not
/// move with it, and a row whose `matched_count` is zero is still served.
#[test]
fn a_filter_adds_a_count_per_row_and_moves_nothing_else() {
    let fx = fixture(None);
    for credential in [full_coverage_credential(), subset_credential()] {
        let broad = credential == full_coverage_credential();
        let plain = browse(&fx.engine, &credential, BrowseForm::Roots, None, 100);
        for (clause, oracle) in [
            (
                FilterExpr::Leaf {
                    column: "archive".into(),
                    operand: FilterOperand::Equals(tessera_types::AttrLocalId::new(
                        fx.codes["xx"],
                    )),
                },
                Box::new(|e: u64| archive_of(e) == "xx") as Box<dyn Fn(u64) -> bool>,
            ),
            (
                FilterExpr::Leaf {
                    column: "score".into(),
                    operand: FilterOperand::Range {
                        lo: None,
                        hi: Some(Endpoint {
                            value: Scalar::Int(0),
                            inclusive: false,
                        }),
                    },
                },
                Box::new(|e: u64| score_of(e) < 0) as Box<dyn Fn(u64) -> bool>,
            ),
        ] {
            let filtered = browse(
                &fx.engine,
                &credential,
                BrowseForm::Roots,
                Some(clause.clone()),
                100,
            );
            assert_eq!(
                filtered.artifacts.len(),
                plain.artifacts.len(),
                "a filter serves the same artifacts (I3, I12)"
            );
            let plain_counts: HashMap<u64, u64> = plain
                .artifacts
                .iter()
                .map(|r| (r.tessera_id.raw(), r.masked_count))
                .collect();
            let mut any_positive = false;
            for row in &filtered.artifacts {
                assert_eq!(
                    row.masked_count, plain_counts[&row.tessera_id.raw()],
                    "and the same masked count beside them"
                );
                let key = row.key.clone().unwrap_or_default();
                let base = ROOTS
                    .iter()
                    .find(|(k, _)| *k == key)
                    .expect("a planted root")
                    .1;
                let want = (base..base + 300)
                    .filter(|&e| (broad || subset_sees(e)) && oracle(e))
                    .count() as u64;
                assert_eq!(
                    row.matched_count,
                    Some(want),
                    "{key}: the filtered count is |membership ∩ M_auth ∩ filter|"
                );
                any_positive |= want > 0;
            }
            assert!(any_positive, "a filter matching nothing proves nothing here");
        }

        // A filter matching nothing: every row is still served, with a zero beside it.
        let empty = browse(
            &fx.engine,
            &credential,
            BrowseForm::Roots,
            Some(FilterExpr::Leaf {
                column: "archive".into(),
                operand: FilterOperand::Equals(tessera_engine::filter::UNRESOLVABLE_VALUE),
            }),
            100,
        );
        assert_eq!(empty.artifacts.len(), plain.artifacts.len());
        assert!(
            empty
                .artifacts
                .iter()
                .all(|r| r.matched_count == Some(0) && r.masked_count > 0),
            "a row whose filtered count is zero is still served, with its masked count intact"
        );
    }
}

/// **The refusals name deployment schema; an artifact is never one of them.**
#[test]
fn the_refusals_are_about_schema_and_an_artifact_is_an_empty_page() {
    let fx = fixture(None);
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let ask = |layer: &str, level: Option<u32>, limit: usize, form: BrowseForm| {
        fx.engine.browse(
            &session,
            BrowseRequest {
                view: "s0",
                layer,
                level,
                form,
                filter: None,
                limit,
                cursor: None,
            },
        )
    };
    for (what, result) in [
        ("an unknown layer", ask("clusters/nowhere", None, 10, BrowseForm::Roots)),
        (
            "a level on a one-level kind",
            ask(LAYER, Some(0), 10, BrowseForm::Roots),
        ),
        ("a zero limit", ask(LAYER, None, 0, BrowseForm::Roots)),
    ] {
        match result {
            Err(EngineError::BrowseRefused(_)) => {}
            other => panic!("{what} is the caller's fault, not {other:?}"),
        }
    }
    // An identifier that names nothing, and one that names a point rather than an artifact: an
    // empty page, never a refusal.
    for id in [
        TesseraId::new(0x7777_7777_7777_7777),
        TesseraId::new(0x1234_5678_9abc_def0),
    ] {
        let page = ask(LAYER, None, 10, BrowseForm::Children(id)).expect("answered, not refused");
        assert_eq!(
            page,
            BrowseOut {
                artifacts: Vec::new(),
                parents: Vec::new(),
                next: None,
            },
            "an unknown parent answers an empty page identically to a leaf"
        );
    }
}
