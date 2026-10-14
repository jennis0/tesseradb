//! **A build writes a shard past `[shards] size`**: shard `k` takes the items the
//! allocation order numbers `[k·size, (k+1)·size)`, each at its number less `k·size`, and every
//! per-entity file of shard `k` is the one-shard build's restricted to those items and renumbered.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float32Array, Float64Array, ListBuilder, StringArray, StringBuilder, UInt32Array,
    UInt64Array,
};

use common::write;
use croaring::Bitmap;
use mosaica_authz::{PostingRef, PostingsReader};
use mosaica_build::{build, build_routed, BuildArgs, ExtentRoute};
use mosaica_filter::{Access, RecordBlob, RecordValue, SortedDict, ValueColumn};
use mosaica_spatial::Bounds;
use mosaica_store::read::{open_bundle, Bundle, ColumnsRef, ScalarSlice};
use mosaica_store::unique::{UniqueIndexes, UniqueKey};
use mosaica_store::ShardLayout;
use mosaica_types::{EntityId, ItemHigh, ShardId, Tenancy};

/// Items in the corpus.
const ITEMS: u64 = 300;

fn strings<S: AsRef<str>>(values: &[Option<S>]) -> ArrayRef {
    Arc::new(StringArray::from(
        values.iter().map(|v| v.as_ref().map(|s| s.as_ref())).collect::<Vec<_>>(),
    ))
}

/// One item's access term: the label is the item's, so every file carrying it agrees.
fn access(id: u64) -> &'static str {
    ["public", "staff", "board"][(id % 3) as usize]
}

/// A points file over `ids`, every column the declaration reads from it.
fn points(path: &Path, ids: &[u64]) {
    let at = |id: u64, step: u64| ((id * step) % 997) as f64 + 0.25;
    write(
        path,
        vec![
            ("entity_id", Arc::new(UInt64Array::from(ids.to_vec())) as ArrayRef),
            (
                "x",
                Arc::new(Float64Array::from(ids.iter().map(|&i| at(i, 37)).collect::<Vec<_>>())),
            ),
            (
                "y",
                Arc::new(Float64Array::from(ids.iter().map(|&i| at(i, 91)).collect::<Vec<_>>())),
            ),
            (
                "access",
                strings(&ids.iter().map(|&i| Some(access(i))).collect::<Vec<_>>()),
            ),
            (
                "kw",
                strings(
                    &ids.iter()
                        .map(|&i| (i % 7 != 0).then(|| format!("k{}", i % 11)))
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "dept",
                strings(
                    &ids.iter()
                        .map(|&i| (i % 5 != 0).then(|| format!("d{}", i % 4)))
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "num",
                Arc::new(Float64Array::from(
                    ids.iter()
                        .map(|&i| (i % 13 != 0).then_some(i as f64 / 4.0))
                        .collect::<Vec<_>>(),
                )),
            ),
            (
                "note",
                strings(
                    &ids.iter()
                        .map(|&i| (i % 2 == 0).then(|| format!("river {} stone {}", i % 9, i)))
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "label",
                strings(&ids.iter().map(|&i| Some(format!("item {i}"))).collect::<Vec<_>>()),
            ),
            (
                "score",
                Arc::new(Float32Array::from(
                    ids.iter().map(|&i| Some(i as f32 / 8.0)).collect::<Vec<_>>(),
                )),
            ),
            (
                "memo",
                strings(
                    &ids.iter()
                        .map(|&i| (i % 3 != 1).then(|| format!("hill {} lake {}", i % 5, i % 17)))
                        .collect::<Vec<_>>(),
                ),
            ),
        ],
    );
}

/// Five clusters over every item but each seventh, assigned round the corpus so every cluster
/// has members in every shard of every size the tests build at.
fn clusters(dir: &Path) {
    let keys: Vec<Option<String>> = (0..5).map(|c| Some(format!("c{c}"))).collect();
    write(&dir.join("clusters.parquet"), vec![("key", strings(&keys))]);
    let members: Vec<u64> = (0..ITEMS).filter(|i| i % 7 != 3).collect();
    write(
        &dir.join("clusters_members.parquet"),
        vec![
            (
                "key",
                strings(&members.iter().map(|&i| Some(format!("c{}", i % 5))).collect::<Vec<_>>()),
            ),
            ("entity", Arc::new(UInt64Array::from(members)) as ArrayRef),
        ],
    );
}

/// Two topics, each over every fourth item from its own start: `t0` ranked from all of its
/// members and from a third of them, `t1` from all of its members. Generating sets spread over the
/// corpus reach every shard.
fn topics(dir: &Path) {
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    for ranks in [&["the whole topic", "a third of it"][..], &["the whole topic"]] {
        for rank in ranks {
            contents.values().values().append_value(rank);
            contents.values().append(true);
        }
        contents.append(true);
    }
    write(
        &dir.join("topics.parquet"),
        vec![
            ("key", strings(&["t0", "t1"].map(Some))),
            ("contents", Arc::new(contents.finish()) as ArrayRef),
        ],
    );
    let mut rows: Vec<(&str, Option<u32>, u64)> = Vec::new();
    for (key, start) in [("t0", 0u64), ("t1", 1)] {
        for (position, id) in (start..ITEMS).step_by(4).enumerate() {
            rows.push((key, None, id));
            rows.push((key, Some(0), id));
            if key == "t0" && position % 3 == 0 {
                rows.push((key, Some(1), id));
            }
        }
    }
    write(
        &dir.join("topics_members.parquet"),
        vec![
            ("key", strings(&rows.iter().map(|r| Some(r.0)).collect::<Vec<_>>())),
            (
                "rank",
                Arc::new(UInt32Array::from(rows.iter().map(|r| r.1).collect::<Vec<_>>()))
                    as ArrayRef,
            ),
            (
                "entity",
                Arc::new(UInt64Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>())),
            ),
        ],
    );
}

const DECLARATION: &str = r#"
[sources]
world            = "world.parquet"
points_a         = "points_a.parquet"
points_b         = "points_b.parquet"
points_c         = "points_c.parquet"
clusters         = "clusters.parquet"
topics           = "topics.parquet"
topics_members   = "topics_members.parquet"
clusters_members = "clusters_members.parquet"
departments      = "departments.parquet"

[defaults]
source          = "world"
allocation_view = "world"

[[view]]
name             = "world"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
point_visibility = { field = "access", default = "public" }

[[view_group]]
name             = "slices"
projection       = "none"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
point_visibility = { field = "access", default = "public" }

[[view_group.view]]
key    = "a"
source = "points_a"

[[view_group.view]]
key    = "b"
source = "points_b"

[[view_group.view]]
key    = "c"
source = "points_c"

[[vocabulary]]
name       = "department"
width      = "u16"
value_set  = "open"
visibility = "derived"
source     = "departments"

[[attribute]]
name   = "id"
type   = "u64"
unique = true
field  = "entity_id"

[[attribute]]
name  = "kw"
type  = "keyword"
index = true

[[attribute]]
name       = "dept"
type       = "category"
vocabulary = "department"
index      = true

[[attribute]]
name   = "num"
type   = "f64"
render = true

[[attribute]]
name  = "note"
type  = "text"
index = true

[[attribute]]
name = "label"
type = "keyword"

[[attribute]]
name   = "score"
type   = "f32"
scope  = { group = "slices" }
index  = true
render = true

[[attribute]]
name  = "memo"
type  = "text"
scope = { group = "slices" }
index = true

[[layer]]
name = "clusters"
title = "Clusters"
views = ["world", "slices"]
source = "clusters"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat", prune_children = false }

  [layer.members]
  source = "clusters_members"
  fields = { id = "entity" }

[[layer]]
name = "topics"
title = "Topics"
views = ["world"]
source = "topics"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }

  [layer.members]
  source = "topics_members"
  fields = { id = "entity" }

  [[layer.content.supplied]]
  name = "topic"
  type = "text"
  require_member_visibility = "all"
"#;

/// The corpus's files and its declaration, `[shards]` first where `size` is given, with the view
/// `slices:c` over the items `c` names.
fn corpus(dir: &Path, size: Option<u64>, c: &[u64]) -> PathBuf {
    let all: Vec<u64> = (0..ITEMS).collect();
    points(&dir.join("world.parquet"), &all);
    let evens: Vec<u64> = all.iter().copied().filter(|i| i % 2 == 0).collect();
    points(&dir.join("points_a.parquet"), &evens);
    let thirds: Vec<u64> = all.iter().copied().filter(|i| i % 3 == 0).collect();
    points(&dir.join("points_b.parquet"), &thirds);
    points(&dir.join("points_c.parquet"), c);
    clusters(dir);
    topics(dir);
    // The vocabulary's codes, pinned: a build draws the codes it mints at random.
    write(
        &dir.join("departments.parquet"),
        vec![
            ("key", strings(&["d0", "d1", "d2", "d3"].map(Some))),
            ("code", Arc::new(UInt32Array::from(vec![40u32, 300, 7000, 12345])) as ArrayRef),
        ],
    );
    let sized = match size {
        Some(size) => format!("[shards]\nsize = {size}\n{DECLARATION}"),
        None => DECLARATION.to_string(),
    };
    let path = dir.join(match size {
        Some(size) => format!("corpus-{size}.toml"),
        None => "corpus.toml".to_string(),
    });
    std::fs::write(&path, sized).unwrap();
    path
}

/// The build arguments the CLI derives from the declaration at `path`, writing oracle pairs.
fn args(path: &Path, out: &Path) -> BuildArgs {
    let extent = Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    };
    BuildArgs {
        emit_oracle_pairs: true,
        ..common::args_from_declaration(path, out, extent)
    }
}

/// The corpus built at `size`, or at the default where none is given, into `dir/<name>`, with
/// `slices:c` over every item.
fn built(dir: &Path, size: Option<u64>, name: &str) -> PathBuf {
    let all: Vec<u64> = (0..ITEMS).collect();
    let out = dir.join(name);
    build(&args(&corpus(dir, size, &all), &out)).expect("the build runs");
    out
}

fn layout(shard: u32) -> ShardLayout<'static> {
    ShardLayout::new("default", ShardId::new(shard).unwrap())
}

/// One row of a view's segment: its position, identifier, entity and two render values' bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    morton: u32,
    residual: u32,
    identity: u64,
    entity: u32,
    num: Option<u64>,
    score: Option<u32>,
}

/// One shard of a bundle: its layout and the bundle's prefix.
struct Shard<'a> {
    bundle: &'a Bundle,
    prefix: PathBuf,
    layout: ShardLayout<'static>,
}

impl Shard<'_> {
    fn dir(&self, rel: String) -> PathBuf {
        self.prefix.join(rel)
    }

    fn record(&self) -> &mosaica_store::manifest::ShardRecord {
        &self.bundle.partitions["default"]
            .shard(self.layout.shard)
            .expect("the shard is in the bundle")
            .record
    }

    /// Every term's posting, in term order.
    fn postings(&self, path: &str) -> Vec<Vec<u32>> {
        let reader = PostingsReader::open(&self.dir(path.to_string()), false).unwrap();
        (0..reader.term_count())
            .map(|term| match reader.posting_at(term).unwrap() {
                None => Vec::new(),
                Some(PostingRef::Array(bytes)) => bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_le_bytes(*chunk))
                    .collect(),
                Some(PostingRef::Roaring(view)) => view.iter().collect(),
            })
            .collect()
    }

    /// The entity holding each `id`, where this shard holds it.
    fn ids(&self) -> BTreeMap<u64, u32> {
        let partition = &self.bundle.partitions["default"];
        let indexes = UniqueIndexes::open(
            &self.bundle.manifest,
            &partition.manifest,
            self.record(),
            &self.prefix,
            None,
        )
        .unwrap();
        let index = indexes.get("id").unwrap();
        let keys: Vec<UniqueKey> = (0..ITEMS).map(UniqueKey::unsigned).collect();
        index
            .lookup(&keys)
            .unwrap()
            .into_iter()
            .map(|(at, entity)| (at as u64, entity))
            .collect()
    }

    /// A keyword column's value at each entity that has one, as its key.
    fn keywords(&self, rel: String) -> Vec<Option<String>> {
        let dir = self.dir(rel);
        let dict = SortedDict::open_dir(&dir, Access::Read).unwrap();
        let values = ValueColumn::open_dir(&dir, Access::Read).unwrap();
        (0..self.record().high_water as u32)
            .map(|entity| {
                values.record_value_of(entity).map(|value| match value {
                    RecordValue::U32(ordinal) => {
                        dict.key_of(ordinal, &mut Vec::new()).unwrap().to_string()
                    }
                    other => panic!("a keyword's value is an ordinal, not {other:?}"),
                })
            })
            .collect()
    }

    /// A value column's value at each entity.
    fn values(&self, rel: String) -> Vec<Option<RecordValue>> {
        let values = ValueColumn::open_dir(&self.dir(rel), Access::Read).unwrap();
        (0..self.record().high_water as u32)
            .map(|entity| values.record_value_of(entity))
            .collect()
    }

    /// A text index as each token's posting.
    fn tokens(&self, rel: String) -> BTreeMap<String, Vec<u32>> {
        let dir = self.dir(rel.clone());
        let dict = SortedDict::open_dir(&dir, Access::Read).unwrap();
        let postings = self.postings(&format!("{rel}/postings.arrow"));
        (0..dict.len())
            .map(|ordinal| {
                let key = dict.key_of(ordinal, &mut Vec::new()).unwrap().to_string();
                (key, postings[ordinal as usize].clone())
            })
            .collect()
    }

    /// Each row of a view, in row order.
    fn rows(&self, view: &str) -> Vec<Row> {
        let segment = self.dir(self.layout.segment_rel(view, "seg-0"));
        let words = |path: PathBuf| -> Vec<u32> {
            std::fs::read(path)
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| u32::from_le_bytes(*chunk))
                .collect()
        };
        let morton = words(segment.join("morton.u32"));
        let entities = words(self.dir(self.layout.view_rel(view)).join("row-entity.u32"));
        let columns = ColumnsRef::load(&segment.join("columns.arrow")).unwrap();
        let num = |row: usize| match columns.scalar("num") {
            Some(ScalarSlice::F64(values)) => Some(values[row].to_bits()),
            _ => None,
        };
        let score = |row: usize| match columns.scalar("score") {
            Some(ScalarSlice::F32(values)) => Some(values[row].to_bits()),
            _ => None,
        };
        (0..morton.len())
            .map(|row| Row {
                morton: morton[row],
                residual: columns.residual()[row],
                identity: columns.mosaica_id()[row],
                entity: entities[row],
                num: num(row),
                score: score(row),
            })
            .collect()
    }
}

fn shard<'a>(bundle: &'a Bundle, root: &Path, id: u32) -> Shard<'a> {
    Shard {
        bundle,
        prefix: common::prefix_dir(root),
        layout: layout(id),
    }
}

/// `whole`'s run of entities in `[lo, hi)`, each less `lo`.
fn cut(whole: &[u32], lo: u32, hi: u32) -> Vec<u32> {
    whole
        .iter()
        .filter(|&&e| (lo..hi).contains(&e))
        .map(|&e| e - lo)
        .collect()
}

/// A member pack's slices: each artifact's membership and generating sets, `None` for a hole.
type Slices = Vec<Option<(Bitmap, Vec<Bitmap>)>>;

/// Every member pack of a shard, by `(space, level, ordinal_lo)`.
fn packs(shard: &Shard<'_>) -> BTreeMap<(u32, u32, u32), Slices> {
    shard
        .record()
        .membership_extents
        .iter()
        .map(|extent| {
            let pack = mosaica_store::membership::LevelPack::open(
                &shard.dir(extent.path.clone()),
                mosaica_store::membership::PackKind::Members,
            )
            .unwrap();
            let slices = pack
                .iter()
                .map(|(_, blob)| {
                    (!blob.is_empty())
                        .then(|| mosaica_lifecycle::membership::decode_members(blob).unwrap())
                })
                .collect();
            ((extent.space.raw(), extent.level, extent.ordinal_lo), slices)
        })
        .collect()
}

/// Every containment partition of a shard, by `(space, level)`: each `(ordinal, rank)`'s set size.
fn containment(shard: &Shard<'_>) -> BTreeMap<(u32, u32), Vec<Vec<u32>>> {
    shard
        .record()
        .derived_extents
        .iter()
        .filter(|extent| extent.form == mosaica_store::manifest::DerivedForm::Containment)
        .map(|extent| {
            let pack =
                mosaica_store::membership::ContainmentPack::open(&shard.dir(extent.path.clone()))
                    .unwrap();
            let sizes = (0..pack.ordinals() as usize)
                .map(|ordinal| {
                    (pack.at(ordinal)..pack.at(ordinal + 1))
                        .map(|at| {
                            let expression = pack.id(at as usize) as usize;
                            pack.word(pack.expression_at(expression) as usize)
                        })
                        .collect()
                })
                .collect();
            ((extent.space.raw(), extent.level), sizes)
        })
        .collect()
}

/// Each derived file a shard holds, by its view, space, level and kind.
fn derived(shard: &Shard<'_>) -> BTreeSet<(Option<String>, u32, u32, String)> {
    shard
        .record()
        .derived_extents
        .iter()
        .map(|e| (e.view.clone(), e.space.raw(), e.level, e.form.dir().to_string()))
        .collect()
}

/// Each layer's recorded layouts, by name.
fn layouts(bundle: &Bundle) -> Vec<(String, String)> {
    bundle.partitions["default"]
        .manifest
        .layers
        .iter()
        .map(|layer| (layer.declaration.name.clone(), format!("{:?}", layer.layouts)))
        .collect()
}

/// `whole` cut to `[lo, hi)`, each less `lo`.
fn cut_set(whole: &Bitmap, lo: u32, hi: u32) -> Bitmap {
    Bitmap::of(&cut(&whole.iter().collect::<Vec<_>>(), lo, hi))
}

/// Each shard of a build at `SIZE` against the one-shard build: every per-entity file, member
/// pack and view row is the one-shard one's slice, renumbered; containment sizes sum to the
/// one-shard sizes; both bundles verify.
fn split_property(route: ExtentRoute) {
    const SIZE: u64 = 64;
    let tmp = tempfile::tempdir().unwrap();
    let build_at = |size: Option<u64>, name: &str, c: &[u64]| {
        let out = tmp.path().join(name);
        let report = build_routed(&args(&corpus(tmp.path(), size, c), &out), route)
            .expect("the build runs");
        if route == ExtentRoute::Extents {
            assert!(report.spilled_columns.contains(&"kw".to_string()), "kw spills");
        }
        out
    };
    // `slices:c` holds a dozen items of shard 0 alone, chosen from where a build numbers them.
    let all: Vec<u64> = (0..ITEMS).collect();
    let probe_root = build_at(None, "probe", &all);
    let probe = open_bundle(&probe_root).unwrap();
    let c: Vec<u64> = shard(&probe, &probe_root, 0)
        .ids()
        .into_iter()
        .filter(|&(_, entity)| u64::from(entity) < SIZE)
        .map(|(id, _)| id)
        .take(12)
        .collect();
    let one_root = build_at(None, "one", &c);
    let split_root = build_at(Some(SIZE), "split", &c);
    let one_bundle = open_bundle(&one_root).expect("the one-shard bundle opens");
    let split_bundle = open_bundle(&split_root).expect("a bundle of several shards opens");

    let count = ITEMS.div_ceil(SIZE) as u32;
    let partition = &split_bundle.partitions["default"];
    assert_eq!(partition.shard_size, SIZE);
    let records: Vec<(u32, u64, u64)> = partition
        .shards
        .iter()
        .map(|shard| (shard.record.id.raw(), shard.record.high_water, shard.record.watermark))
        .collect();
    let expected: Vec<(u32, u64, u64)> = (0..count)
        .map(|k| {
            let held = (ITEMS - u64::from(k) * SIZE).min(SIZE);
            (k, held, held)
        })
        .collect();
    assert_eq!(records, expected, "shard k holds the items numbered [k·size, (k+1)·size)");
    assert_eq!(layouts(&split_bundle), layouts(&one_bundle), "one layout per level");

    let one = shard(&one_bundle, &one_root, 0);
    let global = one.ids();
    assert_eq!(global.len() as u64, ITEMS);
    let one_postings = one.postings(&one.layout.postings_rel());
    let one_terms = mosaica_store::EntityTerms::open_dir(&one.dir(one.layout.entity_terms_rel()))
        .unwrap();
    let one_blob = RecordBlob::open_dir(&one.dir(one.layout.record_rel()), Access::Read).unwrap();
    let one_kw = one.keywords(one.layout.column_rel("kw"));
    let one_dept = one.values(one.layout.column_rel("dept"));
    let one_note = one.tokens(one.layout.column_rel("note"));
    let views = ["world", "slices:a", "slices:b", "slices:c"];
    let slices = &views[1..];
    let one_rows: Vec<_> = views.iter().map(|view| one.rows(view)).collect();
    let scoped = |shard: &Shard<'_>, name: &str, view: &str| {
        shard
            .layout
            .scoped_column_rel(name, view, mosaica_types::view::DECLARED_INCARNATION)
    };
    let one_score: Vec<_> = slices
        .iter()
        .map(|view| one.values(scoped(&one, "score", view)))
        .collect();
    let one_memo: Vec<_> = slices
        .iter()
        .map(|view| one.tokens(scoped(&one, "memo", view)))
        .collect();
    let one_packs = packs(&one);
    assert!(
        one_packs
            .values()
            .flatten()
            .flatten()
            .any(|(_, sets)| !sets.is_empty()),
        "a layer publishes generating sets"
    );
    let spans_shards = |set: &Bitmap| {
        set.iter()
            .map(|entity| u64::from(entity) / SIZE)
            .collect::<BTreeSet<_>>()
            .len()
            > 1
    };
    assert!(
        one_packs
            .values()
            .flatten()
            .flatten()
            .any(|(_, sets)| sets.iter().any(spans_shards)),
        "a generating set spans shards"
    );
    let one_containment = containment(&one);
    assert!(!one_containment.is_empty(), "a level composes its containment partition");
    let mut sizes: BTreeMap<(u32, u32), Vec<Vec<u32>>> = one_containment
        .iter()
        .map(|(level, ranks)| (*level, ranks.iter().map(|r| vec![0; r.len()]).collect()))
        .collect();
    let one_derived = derived(&one);
    assert!(one_derived.len() > one_containment.len(), "the levels derive row-space files");

    let mut seen = 0u64;
    let mut rows_seen = vec![Vec::new(); views.len()];
    for k in 0..count {
        let (lo, hi) = ((k as u64 * SIZE) as u32, ((k as u64 + 1) * SIZE).min(ITEMS) as u32);
        let split = shard(&split_bundle, &split_root, k);
        let at = |entity: u32| entity as usize + lo as usize;

        for (id, local) in split.ids() {
            assert_eq!(lo + local, global[&id], "item {id} is at its number less {lo}");
            seen += 1;
        }

        let postings = split.postings(&split.layout.postings_rel());
        assert_eq!(postings.len(), one_postings.len(), "every shard carries every term");
        for (term, posting) in postings.iter().enumerate() {
            assert_eq!(posting, &cut(&one_postings[term], lo, hi), "term {term} in shard {k}");
        }

        let terms =
            mosaica_store::EntityTerms::open_dir(&split.dir(split.layout.entity_terms_rel()))
                .unwrap();
        let blob =
            RecordBlob::open_dir(&split.dir(split.layout.record_rel()), Access::Read).unwrap();
        for local in 0..hi - lo {
            let entity = lo + local;
            assert_eq!(
                terms.terms_of(local).unwrap(),
                one_terms.terms_of(entity).unwrap()
            );
            assert_eq!(
                blob.fields_of(local).unwrap(),
                one_blob.fields_of(entity).unwrap(),
                "entity {entity}'s record"
            );
        }

        let kw = split.keywords(split.layout.column_rel("kw"));
        let dept = split.values(split.layout.column_rel("dept"));
        for local in 0..(hi - lo) as usize {
            assert_eq!(kw[local], one_kw[at(local as u32)]);
            assert_eq!(dept[local], one_dept[at(local as u32)]);
        }
        let tokens_cut = |whole: &BTreeMap<String, Vec<u32>>| {
            whole
                .iter()
                .map(|(token, posting)| (token.clone(), cut(posting, lo, hi)))
                .filter(|(_, posting)| !posting.is_empty())
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            split.tokens(split.layout.column_rel("note")),
            tokens_cut(&one_note),
            "a shard's text index holds its items' tokens"
        );
        for (index, view) in slices.iter().enumerate() {
            let values = split.values(scoped(&split, "score", view));
            for (local, value) in values.iter().enumerate() {
                assert_eq!(value, &one_score[index][at(local as u32)], "score in {view}");
            }
            assert_eq!(
                split.tokens(scoped(&split, "memo", view)),
                tokens_cut(&one_memo[index]),
                "memo in {view}"
            );
        }

        let split_packs = packs(&split);
        assert_eq!(
            split_packs.keys().collect::<Vec<_>>(),
            one_packs.keys().collect::<Vec<_>>(),
            "every shard holds every pack, over the same ordinals"
        );
        for (key, whole) in &one_packs {
            for (part, whole) in split_packs[key].iter().zip(whole) {
                let expected = whole.as_ref().map(|(members, sets)| {
                    (
                        cut_set(members, lo, hi),
                        sets.iter().map(|set| cut_set(set, lo, hi)).collect(),
                    )
                });
                assert_eq!(part, &expected, "a membership and its generating sets in shard {k}");
            }
        }
        for (level, ranks) in containment(&split) {
            let whole = &one_containment[&level];
            assert_eq!(ranks.len(), whole.len(), "one slot per ordinal in shard {k}");
            for (ordinal, ranks) in ranks.iter().enumerate() {
                assert_eq!(ranks.len(), whole[ordinal].len(), "one entry per rank");
                for (rank, size) in ranks.iter().enumerate() {
                    sizes.get_mut(&level).unwrap()[ordinal][rank] += size;
                }
            }
        }
        assert_eq!(derived(&split), one_derived, "shard {k} derives the one-shard build's files");

        let key = common::test_key();
        let high = ItemHigh::new(ShardId::new(k).unwrap(), Tenancy::ZERO);
        for (index, view) in views.iter().enumerate() {
            let rows = split.rows(view);
            assert!(
                rows.windows(2).all(|pair| {
                    (pair[0].morton, pair[0].identity) < (pair[1].morton, pair[1].identity)
                }),
                "{view} in shard {k} is sorted by (Morton, mosaica_id)"
            );
            for row in &rows {
                let identity = key.forward(high, EntityId::new(u64::from(row.entity))).unwrap();
                assert_eq!(row.identity, identity.raw(), "the identifier is the shard's own");
                rows_seen[index].push(Row {
                    identity: 0,
                    entity: row.entity + lo,
                    ..*row
                });
            }
        }
        let rows_of_c = split
            .record()
            .segments
            .iter()
            .find(|segment| segment.view == "slices:c")
            .unwrap()
            .row_count;
        match k {
            0 => assert_eq!(rows_of_c as usize, c.len()),
            _ => assert_eq!(rows_of_c, 0, "slices:c has no row in shard {k}"),
        }
    }
    assert_eq!(seen, ITEMS, "every item is in exactly one shard");
    assert_eq!(sizes, one_containment, "the shards' set sizes sum to the one-shard sizes");
    for (index, view) in views.iter().enumerate() {
        let mut whole: Vec<Row> = one_rows[index]
            .iter()
            .map(|row| Row {
                identity: 0,
                ..*row
            })
            .collect();
        whole.sort_unstable();
        rows_seen[index].sort_unstable();
        assert_eq!(rows_seen[index], whole, "{view}'s rows are the one-shard view's");
    }

    for root in [&one_root, &split_root] {
        let report = mosaica_build::verify(root).expect("the bundle verifies");
        assert_eq!(report.entity_id_high_water, ITEMS);
        let deep = mosaica_build::verify_deep(root, &Default::default())
            .expect("the bundle verifies deep");
        assert!(deep.artifact_slices > 0, "the artifacts' slices are checked");
    }
}

#[test]
fn a_build_past_its_size_is_the_one_shard_build_split_at_multiples_of_it() {
    split_property(ExtentRoute::Derived);
}

/// The same with every string column that may spill its characters as extents spilling them.
#[test]
fn a_build_past_its_size_is_the_split_one_shard_build_when_every_column_spills() {
    split_property(ExtentRoute::Extents);
}

/// Exactly `size` items is one shard; one item more is a second shard holding that item.
#[test]
fn one_item_past_the_size_opens_a_second_shard_of_one() {
    let tmp = tempfile::tempdir().unwrap();
    for (size, held) in [(ITEMS, vec![ITEMS]), (ITEMS - 1, vec![ITEMS - 1, 1])] {
        let root = built(tmp.path(), Some(size), &format!("at-{size}"));
        let bundle = open_bundle(&root).expect("the bundle opens");
        let high_waters: Vec<u64> = bundle.partitions["default"]
            .shards
            .iter()
            .map(|shard| shard.record.high_water)
            .collect();
        assert_eq!(high_waters, held, "at size {size}");
        mosaica_build::verify(&root).expect("the bundle verifies");
    }
}
