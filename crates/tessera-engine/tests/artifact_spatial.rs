//! **A boundary is the points inside it, decided at request time and never stale.**
//!
//! A `membership = "spatial"` layer stores no membership at all. Each artifact declares a box; its
//! members are the depth-`d` Morton tiles that cover that box, resolved against whatever segments
//! the generation happens to hold. Ruling R3 makes that exact rather than approximate — **the
//! ranges are the membership and the polygon is content** — so a point inside a covering tile is a
//! member whether or not it is inside the box, and the answer is a set rather than an estimate.
//!
//! **The oracle is the corpus generator, not this file.** Every count asserted below is computed by
//! quantising the generator's own points to depth-`d` tiles and masking by the principal's grant —
//! the same two operations the engine performs through entirely different code, over segments on
//! disk instead of over a closed form. A defect in either shows up as a disagreement.
//!
//! **The boxes are the fixture's**, not the generator's: `tessera-corpus`'s boundary arm carries a
//! roster of authored tile prefixes and no geometry, because it was written while the machinery
//! that would read one did not exist. So this file builds a box per chosen tile — the middle half
//! of it, asserted to cover exactly that tile and no other — and uses the generator only for the
//! points, the access relation and the membership oracle.
//!
//! What is checked, in order: the counts against the oracle over several principals and viewports;
//! that a point ingested inside a box counts on the next request with nothing rebuilt; that a
//! suppressed member leaves the count and a suppressed artifact leaves the map; that a fold leaves
//! every answer where it was, ranges being row-space-independent; and that an absolute criterion
//! fires on such a layer exactly as it does on a stored one.

mod common;

use std::collections::BTreeMap;

use common::*;
use tessera_corpus::{Corpus, Grant};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::ChangeOp;
use tessera_types::EntityId;

const N: u64 = 3_000;
const SEED: u64 = 0x5EED;
/// The depth the fixture's boxes are covered at. **Part of the membership** — the same boxes at a
/// different depth are a different member set — so it is written once here and read by the oracle
/// and the declaration alike.
const DEPTH: u8 = 3;
const LAYER: &str = "regions/boxes";
const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
/// How many tiles the fixture draws boxes over. Enough that the layer is a real minority of the
/// grid — the shape a boundary set actually has — and few enough to stay a fast test.
const BOXES: usize = 8;
/// The tile depth the narrow viewports are asked at. **A request's tiles come from its own `zoom`**,
/// so at zoom 0 a bbox resolves to the one tile covering the whole grid and narrows nothing — a
/// viewport case asked there would compare the whole map with itself.
const VIEWPORT_ZOOM: u8 = 4;

fn corpus() -> Corpus {
    Corpus::new(SEED, N, extent()).expect("the generator accepts the fixture's extent")
}

fn credential(grant: &str) -> Vec<u8> {
    let terms: Vec<String> = Grant::parse(grant)
        .expect("the grant is inside the generator's term space")
        .terms()
        .iter()
        .map(|t| format!("\"{}\"", t.raw()))
        .collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

/// The depth-[`DEPTH`] tile one position lands in — **the generator's own quantisation**, which is
/// the build's too: the same `cell`/`interleave_bits` pair, so the oracle and the engine cannot
/// disagree about which tile a point is in.
fn tile_of(x: f64, y: f64) -> u64 {
    let e = extent();
    let shift = 16 - u32::from(DEPTH);
    let cx = u32::from(tessera_spatial::cell(x, e.x_min, e.x_max)) >> shift;
    let cy = u32::from(tessera_spatial::cell(y, e.y_min, e.y_max)) >> shift;
    tessera_spatial::interleave_bits(cx, cy, DEPTH)
}

/// The middle half of tile `prefix`, as a box in extent coordinates.
///
/// **The middle half rather than the whole tile**, so the box cannot touch a neighbour: a tile's
/// bounds are half-open and a box that reached them would quantise into the tile beyond. The
/// assertion beside every use is what makes that a fact rather than an intention.
fn box_of(prefix: u64, tx: u32, ty: u32) -> [f64; 4] {
    let e = extent();
    let span = 65536u32 >> DEPTH;
    let at = |cells: u32, min: f64, max: f64| min + f64::from(cells) * (max - min) / 65536.0;
    let quarter = span / 4;
    let bbox = [
        at(tx * span + quarter, e.x_min, e.x_max),
        at(ty * span + quarter, e.y_min, e.y_max),
        at(tx * span + 3 * quarter, e.x_min, e.x_max),
        at(ty * span + 3 * quarter, e.y_min, e.y_max),
    ];
    let covering = tessera_spatial::tiles_for_bbox(bbox, DEPTH, &e);
    assert_eq!(
        covering.len(),
        1,
        "the fixture's box for prefix {prefix} covers {} tiles, not one",
        covering.len()
    );
    assert_eq!(covering[0].prefix, prefix, "and it covers the wrong one");
    bbox
}

/// The tiles this fixture draws boxes over, most populous first — so every artifact has members and
/// the counts below are numbers rather than zeros.
fn chosen_tiles(c: &Corpus) -> Vec<(u64, u32, u32)> {
    let mut population: BTreeMap<u64, u64> = BTreeMap::new();
    for e in 0..c.n() {
        let item = c.item(e);
        *population
            .entry(tile_of(f64::from(item.x), f64::from(item.y)))
            .or_default() += 1;
    }
    let mut ranked: Vec<(u64, u64)> = population.into_iter().collect();
    ranked.sort_by_key(|(prefix, count)| (std::cmp::Reverse(*count), *prefix));
    ranked.truncate(BOXES);
    ranked.sort_by_key(|(prefix, _)| *prefix);

    // The `(tx, ty)` a prefix interleaves from, recovered by enumeration: the grid is `4^DEPTH`
    // tiles, which at this depth is a thousand-odd, and an inverse spread written by hand here
    // would be a second transcription of `interleave_bits`.
    let side = 1u32 << DEPTH;
    ranked
        .into_iter()
        .map(|(prefix, _)| {
            for ty in 0..side {
                for tx in 0..side {
                    if tessera_spatial::interleave_bits(tx, ty, DEPTH) == prefix {
                        return (prefix, tx, ty);
                    }
                }
            }
            panic!("prefix {prefix} is not a depth-{DEPTH} tile")
        })
        .collect()
}

/// The declaration: one spatial layer, its boxes written into the document itself.
fn config_toml(c: &Corpus, criterion: &str) -> String {
    let artifacts: Vec<String> = chosen_tiles(c)
        .into_iter()
        .map(|(prefix, tx, ty)| {
            let bbox = box_of(prefix, tx, ty);
            format!(
                "  {{ key = \"t{prefix}\", bbox = [{}, {}, {}, {}] }},",
                bbox[0], bbox[1], bbox[2], bbox[3]
            )
        })
        .collect();
    format!(
        r#"
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[[view]]
name             = "s0"
extent           = {{ min = 0.0, max = 1000.0 }}
point_visibility = {{ source = "pairs", default = "public" }}

[[layer]]
name                      = "{LAYER}"
views                     = ["s0"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = {criterion}
artifacts = [
{}
]

  [layer.shape]
  kind  = "bbox"
  depth = {DEPTH}
"#,
        artifacts.join("\n")
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
    corpus: Corpus,
}

fn fixture(criterion: &str) -> Fixture {
    let corpus = corpus();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    corpus.write_points_parquet(&points).expect("points");
    corpus.write_pairs_parquet(&pairs).expect("pairs");
    let config_path = tmp.path().join("spatial-config.toml");
    std::fs::write(&config_path, config_toml(&corpus, criterion)).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the spatial fixture's declaration parses");
    build_with_layers(&root, &points, &pairs, &corpus, config);
    let cache = tmp.path().join("cache");
    let wal = tmp.path().join("wal");
    Fixture {
        _tmp: tmp,
        root,
        cache,
        wal,
        corpus,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The oracle: every box's masked count, computed from the generator alone.
    ///
    /// `deleted` is the entities a change has taken away — the same adjustment the artifact census
    /// makes, so the expectation tracks the write cycle rather than only the build.
    fn expected(&self, grant: &str, deleted: &[u64]) -> BTreeMap<String, u64> {
        let g = Grant::parse(grant).unwrap();
        let chosen: Vec<u64> = chosen_tiles(&self.corpus)
            .into_iter()
            .map(|(prefix, _, _)| prefix)
            .collect();
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for e in 0..self.corpus.n() {
            if deleted.contains(&e) || !self.corpus.visible(e, &g) {
                continue;
            }
            let item = self.corpus.item(e);
            let tile = tile_of(f64::from(item.x), f64::from(item.y));
            if chosen.contains(&tile) {
                *counts.entry(format!("t{tile}")).or_default() += 1;
            }
        }
        counts.retain(|_, count| *count > 0);
        counts
    }
}

/// What the layer serves one principal at one viewport, by key.
///
/// **`zoom` is not decoration.** A request's tiles come from `tiles_for_bbox(bbox, zoom, …)`, so at
/// zoom 0 every box resolves to the single tile covering the whole grid and a narrower `bbox`
/// narrows nothing. Every case below that is *about* the viewport therefore asks at a depth where
/// the box means something.
fn served(engine: &Engine, grant: &str, zoom: u8, bbox: [f64; 4]) -> BTreeMap<String, u64> {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [LAYER];
    let mut request = ViewportRequest::new("s0", zoom, bbox, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    engine
        .viewport(&session, request)
        .expect("a viewport over the fixture")
        .artifacts
        .into_iter()
        .map(|artifact| {
            (
                artifact.key.expect("the boxes are keyed"),
                artifact.masked_count,
            )
        })
        .collect()
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Ingest one point at `(x, y)` visible to `term`.
fn ingest_point(engine: &Engine, external_id: &str, term: u32, x: f32, y: f32) {
    let descriptors = vec![term.to_string().into_bytes()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(external_id.as_bytes()) {
        *slot = *byte;
    }
    engine
        .accept_ingest(
            vec![UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                view: "s0".to_string(),
                descriptors: descriptors.clone(),
                x,
                y,
                scalars: Vec::new(),
                terms: engine.resolve_terms(&descriptors),
            }],
            external_id.to_string(),
            hash,
        )
        .expect("an ordinary point is an ordinary write");
}

// ---------------------------------------------------------------------------------------------

/// **The counts are the generator's**, for every principal and every viewport.
///
/// A count is over the whole membership rather than over the viewport (`annotations.md` §4.2), so
/// panning must move which artifacts are served and never the number beside one — which is asserted
/// here as well as the counts themselves, because a count that narrowed with the box would let a
/// viewer difference two boxes for the members in between.
#[test]
fn a_boundarys_masked_count_is_the_one_the_generator_computes() {
    let fx = fixture("\"none\"");
    let engine = fx.open();

    for grant in ["0", "0,1", "1,2,3", "5,6,7,8"] {
        let expected = fx.expected(grant, &[]);
        assert_eq!(
            served(&engine, grant, 0, WHOLE_MAP),
            expected,
            "the served counts differ from the generator's for grant {grant}"
        );
        assert!(
            !expected.is_empty(),
            "grant {grant} sees nothing at all, so nothing is under test"
        );

        // Every narrower viewport serves a subset, at the *same* counts.
        for bbox in [
            [0.0, 0.0, 500.0, 500.0],
            [250.0, 250.0, 750.0, 750.0],
            [0.0, 0.0, 125.0, 125.0],
        ] {
            let narrow = served(&engine, grant, VIEWPORT_ZOOM, bbox);
            for (key, count) in &narrow {
                assert_eq!(
                    expected.get(key),
                    Some(count),
                    "{key}'s count moved with the viewport under grant {grant}"
                );
            }
            assert!(narrow.len() <= expected.len());
        }
    }

    // A viewport in a corner serves strictly fewer boxes than the whole map, or the candidacy
    // arithmetic is not being exercised at all.
    let whole = served(&engine, "0,1", 0, WHOLE_MAP);
    let corner = served(&engine, "0,1", VIEWPORT_ZOOM, [0.0, 0.0, 125.0, 125.0]);
    assert!(
        corner.len() < whole.len(),
        "every box is a candidate in a corner viewport, so the ranges decide nothing"
    );
}

/// **A point ingested inside a boundary is a member on the next request, with nothing rebuilt.**
///
/// Nothing rebuilt is stated as what did not happen: no fold, and no write to the layer at all —
/// the ranges are a function of the shapes and the segments, and a flush publishes a segment. That
/// is the property a stored membership cannot have, and it is why the source exists.
#[test]
fn a_point_ingested_inside_a_boundary_counts_on_the_next_request() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let target = chosen_tiles(&fx.corpus)[0];
    let key = format!("t{}", target.0);
    let bbox = box_of(target.0, target.1, target.2);
    let before = served(&engine, "0", 0, WHOLE_MAP);
    let was = *before.get(&key).expect("the busiest tile has members");
    let folds = engine.write_executor_stats().folds;

    // The centre of the box, which is inside the tile by construction.
    ingest_point(
        &engine,
        "inside-1",
        0,
        ((bbox[0] + bbox[2]) / 2.0) as f32,
        ((bbox[1] + bbox[3]) / 2.0) as f32,
    );
    flush(&engine);

    let after = served(&engine, "0", 0, WHOLE_MAP);
    assert_eq!(
        after.get(&key),
        Some(&(was + 1)),
        "the ingested point did not reach the boundary it landed in"
    );
    assert_eq!(
        engine.write_executor_stats().folds,
        folds,
        "the count moved because of a fold rather than because of the geometry"
    );
    for (other, count) in &after {
        if other != &key {
            assert_eq!(before.get(other), Some(count), "{other} moved too");
        }
    }
}

/// **A fold leaves every answer where it was.** A fold renumbers the base row space wholesale,
/// which is exactly what a stored membership has to be rewritten for; a shape's membership is
/// re-derived over the rows the fold wrote, so nothing about it is a projection that could go
/// stale.
#[test]
fn a_fold_leaves_a_boundarys_answers_unchanged() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let grant = "0,1";
    let before = served(&engine, grant, 0, WHOLE_MAP);
    let before_narrow = served(&engine, grant, VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]);
    assert!(!before.is_empty());

    // Something to fold: a flushed extent above the base, so the fold has rows to renumber.
    ingest_point(&engine, "pre-fold", 0, 3.0, 3.0);
    flush(&engine);
    fold(&engine);

    let after = served(&engine, grant, 0, WHOLE_MAP);
    let after_narrow = served(&engine, grant, VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]);
    for (key, count) in &before {
        // The ingested point may itself have landed in one of the boxes, which is a real +1 rather
        // than a discrepancy — so the assertion is that nothing *lost* members.
        assert!(
            after.get(key).is_some_and(|now| now >= count),
            "{key} lost members across a fold: {count} → {:?}",
            after.get(key)
        );
    }
    assert_eq!(after.len(), before.len(), "a box left the map at the fold");
    assert_eq!(after_narrow.len(), before_narrow.len());
}

/// **A suppressed member leaves the count, and a suppressed artifact leaves the map** — the same
/// two rules a stored membership follows, reached through a membership nothing stores.
#[test]
fn a_deny_reaches_a_boundary_and_its_members() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let grant = "0,1";
    let target = chosen_tiles(&fx.corpus)[0].0;
    let key = format!("t{target}");
    let before = served(&engine, grant, 0, WHOLE_MAP);
    let was = before[&key];
    assert!(was > 2, "the busiest box has too few members to test with");

    // One visible member of that box, suppressed.
    let by_source = source_to_new_map(&fx.root, "v00000");
    let g = Grant::parse(grant).unwrap();
    let victim = (0..fx.corpus.n())
        .find(|e| {
            let item = fx.corpus.item(*e);
            fx.corpus.visible(*e, &g) && tile_of(f64::from(item.x), f64::from(item.y)) == target
        })
        .expect("the box has a visible member");
    engine
        .accept_change(EntityId::new(by_source[&victim]), ChangeOp::Suppress)
        .expect("a suppression is accepted");

    assert_eq!(
        served(&engine, grant, 0, WHOLE_MAP)[&key],
        was - 1,
        "the suppressed member is still counted inside its boundary"
    );

    // And the artifact's own entity: suppressed, it is absent whole.
    let id = served_id(&engine, grant, &key);
    engine
        .accept_change(id, ChangeOp::Suppress)
        .expect("an artifact's own suppression is accepted");
    assert!(
        !served(&engine, grant, 0, WHOLE_MAP).contains_key(&key),
        "a suppressed boundary is still on the map"
    );
}

/// The entity behind a served artifact, found by asking the identifier route to resolve what the
/// viewport handed out — the same address a suppression names.
fn served_id(engine: &Engine, grant: &str, key: &str) -> EntityId {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [LAYER];
    let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    let row = engine
        .viewport(&session, request)
        .expect("a viewport")
        .artifacts
        .into_iter()
        .find(|artifact| artifact.key.as_deref() == Some(key))
        .expect("the key is served");
    // Inverted through the admin plane's own resolver, the way `/control/changes` does — so the
    // suppression below exercises the misdirection guard rather than going round it.
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine
        .resolve_tessera_ids(&[row.tessera_id], idset)
        .unwrap()[0]
        .expect("an artifact identifier names the entity this deployment issued for it")
}

/// **An absolute criterion fires on a shape exactly as it does on a stored set.** The number tested
/// is the number served — one quantity, computed once — so a box a viewer can see too little of is
/// absent rather than served with a small count beside it.
#[test]
fn an_absolute_criterion_fires_on_a_boundary() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let open = served(&engine, "0,1", 0, WHOLE_MAP);
    // **The median of what is served**, so the bar withholds roughly half: a bar under every count
    // would leave the case asserting that a criterion changes nothing.
    let mut counts: Vec<u64> = open.values().copied().collect();
    counts.sort_unstable();
    let bar = counts[counts.len() / 2];
    assert!(bar > 0, "the fixture's counts are too small to set a bar");
    drop(engine);

    let gated = fixture(&format!("{{ count = {bar} }}"));
    let engine = gated.open();
    let served_gated = served(&engine, "0,1", 0, WHOLE_MAP);
    let expected: BTreeMap<String, u64> = gated
        .expected("0,1", &[])
        .into_iter()
        .filter(|(_, count)| *count >= bar)
        .collect();
    assert_eq!(
        served_gated, expected,
        "the criterion tested a different number from the one served"
    );
    assert!(
        served_gated.len() < open.len(),
        "the criterion withheld nothing, so it is untested"
    );
}

/// **A box survives a restart**, which is the one thing about a spatial layer that is durable.
///
/// The ranges are not stored — they are re-derived from the shapes and the segments, so there is
/// nothing about them a restart could lose. What *is* stored is the box itself, in the artifact's
/// own record blob, and an artifact restored without one has no membership rule at all: it would
/// count zero for every viewer and be absent under any criterion, which no client can tell from an
/// artifact whose members are simply invisible to them. So the decoder refuses such a blob, and
/// this asserts the whole path — encode at publication, map at open, decode, re-derive — by asking
/// for the same answers twice across a reopen.
#[test]
fn a_boundarys_box_survives_a_restart() {
    let fx = fixture("\"none\"");
    let (before, before_narrow) = {
        let engine = fx.open();
        (
            served(&engine, "0,1", 0, WHOLE_MAP),
            served(&engine, "0,1", VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]),
        )
    };
    assert!(!before.is_empty());

    let engine = fx.open();
    assert_eq!(
        served(&engine, "0,1", 0, WHOLE_MAP),
        before,
        "a boundary's box did not come back from the extent it was written into"
    );
    assert_eq!(
        served(&engine, "0,1", VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]),
        before_narrow
    );

    // And again after a fold, which rewrites every extent whole — the path a restored box takes
    // through `repack_all` rather than through the publication that first wrote it.
    ingest_point(&engine, "pre-fold", 0, 3.0, 3.0);
    flush(&engine);
    fold(&engine);
    drop(engine);

    let engine = fx.open();
    let after = served(&engine, "0,1", 0, WHOLE_MAP);
    assert_eq!(
        after.len(),
        before.len(),
        "a box was lost by the fold's rewrite"
    );
    for (key, count) in &before {
        assert!(
            after.get(key).is_some_and(|now| now >= count),
            "{key} lost members across a fold and a restart"
        );
    }
}
