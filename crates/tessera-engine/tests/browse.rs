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

use arrow::array::{Float64Array, Int32Array, StringArray, UInt64Array};
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
    write_pairs_n(&pairs, N);
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
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &with_id(schema.clone())),
        out: bundle.clone(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: with_id(schema),
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
    let alpha = id_of(&fx, &full_coverage_credential(), "alpha");
    let entity = fx.engine.resolve_tessera_ids(&[alpha]).unwrap()[0].unwrap();
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

/// **A row's `child_count` is the size of its children form**, counted over the children this
/// principal is served.
///
/// Under a criterion the narrow principal clears only at the roots, the broad principal's roots
/// count three children each and the narrow principal's count none. A child suppressed after the
/// build leaves its parent's count at two. A leaf counts none.
#[test]
fn a_rows_child_count_counts_only_the_children_this_principal_is_served() {
    let fx = fixture(Some(ExistenceCriterion::Count(50)));
    let counts = |credential: &[u8]| -> Vec<(String, u64)> {
        let mut rows: Vec<(String, u64)> =
            browse(&fx.engine, credential, BrowseForm::Roots, None, 100)
                .artifacts
                .into_iter()
                .map(|row| (row.key.unwrap_or_default(), row.child_count))
                .collect();
        rows.sort();
        rows
    };
    let all = |n: u64| {
        vec![
            ("alpha".to_string(), n),
            ("bravo".to_string(), n),
            ("charlie".to_string(), n),
        ]
    };
    assert_eq!(counts(&full_coverage_credential()), all(3));
    assert_eq!(counts(&subset_credential()), all(0));

    let alpha = id_of(&fx, &full_coverage_credential(), "alpha");
    let children = browse(
        &fx.engine,
        &full_coverage_credential(),
        BrowseForm::Children(alpha),
        None,
        100,
    );
    assert_eq!(children.artifacts.len(), 3);
    assert!(
        children.artifacts.iter().all(|row| row.child_count == 0),
        "a leaf counts no children"
    );

    let one = id_of(&fx, &full_coverage_credential(), "alpha-one");
    let entity = fx.engine.resolve_tessera_ids(&[one]).unwrap()[0].unwrap();
    fx.engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .unwrap();
    let after = counts(&full_coverage_credential());
    assert_eq!(after[0], ("alpha".to_string(), 2), "the suppressed child is not counted");
    assert_eq!(counts(&subset_credential()), all(0));
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

/// **A session established before a fold counts none of the entity the fold retired** — the same
/// session object, never re-authorised, over a filtered browse.
///
/// A fold retires the deletion's tombstone and rotates the bundle identity without moving the
/// watermark, so this session's frozen fragment still names the retired entity and the overlay no
/// longer denies it. A filtered browse composes that fragment into an entity-space candidate, and
/// a row's `matched_count` is what a retired entity would reappear in.
#[test]
fn a_session_from_before_a_fold_counts_none_of_the_entity_the_fold_retired() {
    let fx = fixture(None);
    // A refresh pass rebuilds each resident session's fragment, which would make a request path
    // that failed to notice the rotation indistinguishable from one that noticed.
    fx.engine.set_background_refresh_for_test(false);
    let session = fx
        .engine
        .authorise(&full_coverage_credential())
        .expect("the credential resolves");

    // Source 0 is `alpha`'s first member and carries `xx`; `charlie` is the untouched control.
    let doomed = EntityId::new(source_to_new_map(&fx._dir.path().join("bundle"), "v00000")[&0]);
    assert_eq!(archive_of(0), "xx");
    let xx = FilterExpr::Leaf {
        column: "archive".into(),
        operand: FilterOperand::Equals(tessera_types::AttrLocalId::new(fx.codes["xx"])),
    };
    let counted = |key: &str| -> u64 {
        let out = fx
            .engine
            .browse(
                &session,
                BrowseRequest {
                    view: "s0",
                    layer: LAYER,
                    level: None,
                    form: BrowseForm::Roots,
                    filter: Some(xx.clone()),
                    limit: 100,
                    cursor: None,
                },
            )
            .expect("a filtered browse answers");
        out.artifacts
            .iter()
            .find(|row| row.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("'{key}' is served"))
            .matched_count
            .expect("a filtered row carries a matched count")
    };

    let alpha_before = counted("alpha");
    let charlie_before = counted("charlie");
    assert!(alpha_before > 1, "the count must have room to fall by one");

    fx.engine
        .accept_change(doomed, tessera_lifecycle::wal::ChangeOp::Delete)
        .expect("a delete is accepted");
    let before = fx.engine.write_executor_stats();
    fx.engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = fx.engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        fx.engine.overlay_depth(),
        0,
        "the tombstone retired, so nothing but the folded corpus hides the entity now"
    );
    // The premise, without which the assertions below hold for the wrong reason.
    assert!(
        session.fragment_at_authorise_for_test().view().contains(doomed.raw() as u32),
        "the frozen fragment must still name the retired entity"
    );

    assert_eq!(
        counted("alpha"),
        alpha_before - 1,
        "the filtered count moved by exactly the retired entity"
    );
    assert_eq!(
        counted("charlie"),
        charlie_before,
        "and an artifact the fold did not touch counts what it counted"
    );
}

/// `clusters/bare`, a layer whose artifacts carry no text, over the browse fixture, and the label
/// layers attached to it.
///
/// - `labels/a` serves a label while one of its own members is visible. Its label on `b1` holds
///   only members the subset viewer cannot see. It holds two labels on `b4`, published `a4-z`
///   first, and the viewport's order within a level puts `a4-a` first by key.
/// - `labels/b` comes after `labels/a` in the order `/v1/meta` lists layers, so it names only the
///   clusters `labels/a` leaves unnamed.
/// - `labels/gated` is behind the layer label `1`, which the subset viewer holds and the broad
///   viewer does not.
const BARE: &str = "clusters/bare";

struct Labelled {
    fx: Fixture,
    map: std::collections::BTreeMap<u64, u64>,
    /// The label artifacts' identifiers, by key.
    ids: HashMap<String, TesseraId>,
}

impl Labelled {
    fn members(&self, sources: impl IntoIterator<Item = u64>) -> Vec<EntityId> {
        sources
            .into_iter()
            .map(|s| EntityId::new(self.map[&s]))
            .collect()
    }

    fn label(
        &self,
        key: &str,
        target: &str,
        text: &str,
        sources: impl IntoIterator<Item = u64>,
    ) -> IncomingArtifact {
        let mut label = IncomingArtifact::from_entities(Some(key.into()), self.members(sources));
        label.attached_to = Some(tessera_lifecycle::membership::IncomingAttachment {
            layer: BARE.into(),
            level: 0,
            key: target.into(),
        });
        label.contents = vec![tessera_lifecycle::membership::IncomingContent {
            values: vec![text.into()],
            generated_from: Default::default(),
        }];
        label
    }

    fn publish(&mut self, layer: &str, labels: Vec<IncomingArtifact>) {
        let keys: Vec<String> = labels.iter().map(|l| l.key.clone().unwrap()).collect();
        let ids = self
            .fx
            .engine
            .publish_artifacts(layer.into(), 0, labels)
            .unwrap();
        self.ids.extend(keys.into_iter().zip(ids));
        tick(&self.fx.engine);
    }

    fn ask(&self, credential: &[u8], form: BrowseForm, filter: Option<FilterExpr>) -> BrowseOut {
        let session = self.fx.engine.authorise(credential).unwrap();
        self.fx
            .engine
            .browse(
                &session,
                BrowseRequest {
                    view: "s0",
                    layer: BARE,
                    level: None,
                    form,
                    filter,
                    limit: 100,
                    cursor: None,
                },
            )
            .expect("a browse answers")
    }

    fn names(&self, credential: &[u8]) -> Vec<(String, Option<String>)> {
        named_rows(&self.ask(credential, BrowseForm::Roots, None))
    }

    /// The keys a search finds, and that the answer equals one for text nobody wrote where it
    /// finds nothing.
    fn search(&self, credential: &[u8], q: &str) -> Vec<String> {
        let found = self.ask(credential, BrowseForm::Search(q.into()), None);
        if found.artifacts.is_empty() {
            assert_eq!(
                found,
                self.ask(
                    credential,
                    BrowseForm::Search("no such text anywhere".into()),
                    None
                ),
                "a search finding nothing answers as text nobody wrote"
            );
        }
        key_set(&found)
    }
}

fn named_rows(out: &BrowseOut) -> Vec<(String, Option<String>)> {
    let mut names: Vec<_> = out
        .artifacts
        .iter()
        .map(|row| (row.key.clone().unwrap_or_default(), row.name.clone()))
        .collect();
    names.sort();
    names
}

fn named(pairs: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    pairs
        .iter()
        .map(|(key, name)| (key.to_string(), name.map(str::to_string)))
        .collect()
}

fn labelled() -> Labelled {
    let fx = fixture(None);
    let map = source_to_new_map(&fx._dir.path().join("bundle"), "v00000");
    let mut lx = Labelled {
        fx,
        map,
        ids: HashMap::new(),
    };
    let mut bare = declaration(None);
    bare.name = BARE.into();
    bare.hierarchy.kind = HierarchyKind::Flat;
    bare.content = ContentDeclaration::default();
    lx.fx.engine.register_layer(bare).unwrap();
    let clusters: Vec<IncomingArtifact> = (0..6u64)
        .map(|b| {
            IncomingArtifact::from_entities(
                Some(format!("b{b}")),
                lx.members(b * 100..b * 100 + 100),
            )
        })
        .collect();
    lx.fx
        .engine
        .publish_artifacts(BARE.into(), 0, clusters)
        .unwrap();
    for (name, visibility) in [
        ("labels/a", None),
        ("labels/b", None),
        ("labels/gated", Some("1")),
    ] {
        let mut declared = declaration(Some(ExistenceCriterion::Count(1)));
        declared.name = name.into();
        declared.visibility = visibility.map(str::to_string);
        declared.hierarchy.kind = HierarchyKind::Flat;
        declared.depends_on = vec![BARE.into()];
        lx.fx.engine.register_layer(declared).unwrap();
    }
    // Source ids 100..130 not divisible by three: members only the broad viewer sees.
    let hidden: Vec<u64> = (100..130).filter(|&s| !subset_sees(s)).collect();
    let a = vec![
        lx.label("a0", "b0", "Spin magnetic effect", 0..30),
        lx.label("a1", "b1", "Quantum dots", hidden),
        lx.label("a2", "b2", "Alpha topic", 200..230),
        lx.label("a4-z", "b4", "Published first", 400..430),
        lx.label("a4-a", "b4", "First by key", 400..430),
    ];
    lx.publish("labels/a", a);
    let b = vec![
        lx.label("x2", "b2", "Beta topic", 200..230),
        lx.label("x3", "b3", "Only topic", 300..330),
    ];
    lx.publish("labels/b", b);
    let gated = vec![lx.label("g5", "b5", "Gated topic", 500..530)];
    lx.publish("labels/gated", gated);
    lx
}

/// **A row with no text of its own is named by the label attached to it, where this viewer is
/// served that label, and the search form reads that name.**
#[test]
fn an_attached_label_names_a_row_only_where_this_viewer_is_served_it() {
    let lx = labelled();
    let broad = full_coverage_credential();
    let subset = subset_credential();
    assert_eq!(
        lx.names(&broad),
        named(&[
            ("b0", Some("Spin magnetic effect")),
            ("b1", Some("Quantum dots")),
            ("b2", Some("Alpha topic")),
            ("b3", Some("Only topic")),
            ("b4", Some("First by key")),
            ("b5", None),
        ]),
        "the first layer names a row, and within it the first by key"
    );
    assert_eq!(
        lx.names(&subset),
        named(&[
            ("b0", Some("Spin magnetic effect")),
            ("b1", None),
            ("b2", Some("Alpha topic")),
            ("b3", Some("Only topic")),
            ("b4", Some("First by key")),
            ("b5", Some("Gated topic")),
        ]),
        "a label the viewer is not served names nothing"
    );
    for credential in [&broad, &subset] {
        assert_eq!(lx.search(credential, "spin MAGNETIC"), vec!["b0"]);
        assert!(
            lx.search(credential, "beta").is_empty(),
            "a label that is not the name"
        );
        assert!(lx.search(credential, "published first").is_empty());
    }
    assert_eq!(lx.search(&broad, "quantum"), vec!["b1"]);
    assert!(
        lx.search(&subset, "quantum").is_empty(),
        "a label withheld by its members"
    );
    assert_eq!(lx.search(&subset, "gated"), vec!["b5"]);
    assert!(
        lx.search(&broad, "gated").is_empty(),
        "a label layer behind a label not held"
    );
}

/// **A filter neither withholds a label nor reveals one**: labels gate on the visible set.
#[test]
fn a_filter_moves_no_name() {
    let lx = labelled();
    // Matches no item at all.
    let filter = FilterExpr::Leaf {
        column: "score".into(),
        operand: FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(1000),
                inclusive: true,
            }),
            hi: None,
        },
    };
    for credential in [full_coverage_credential(), subset_credential()] {
        let plain = named_rows(&lx.ask(&credential, BrowseForm::Roots, None));
        let filtered = named_rows(&lx.ask(&credential, BrowseForm::Roots, Some(filter.clone())));
        assert_eq!(filtered, plain);
        let q = || BrowseForm::Search("quantum".into());
        assert_eq!(
            key_set(&lx.ask(&credential, q(), Some(filter.clone()))),
            key_set(&lx.ask(&credential, q(), None))
        );
    }
}

/// **A suppressed label names nothing and is not found**, and a label published after a browse
/// names its target on the next. An attachment is fixed once published, so a publication is the
/// only write that adds one.
#[test]
fn a_write_to_a_label_layer_moves_the_names_on_the_next_browse() {
    let mut lx = labelled();
    let broad = full_coverage_credential();
    assert_eq!(lx.search(&broad, "only topic"), vec!["b3"]);

    let x3 = artifact_entity(&lx.fx.engine, lx.ids["x3"]);
    lx.fx
        .engine
        .accept_change(x3, tessera_lifecycle::wal::ChangeOp::Suppress)
        .unwrap();
    let names = lx.names(&broad);
    assert!(names.contains(&("b3".to_string(), None)), "{names:?}");
    assert!(lx.search(&broad, "only topic").is_empty());

    let late = vec![lx.label("x5", "b5", "Late topic", 500..530)];
    lx.publish("labels/b", late);
    let names = lx.names(&broad);
    assert!(
        names.contains(&("b5".to_string(), Some("Late topic".to_string()))),
        "{names:?}"
    );
    assert_eq!(lx.search(&broad, "late topic"), vec!["b5"]);
}
