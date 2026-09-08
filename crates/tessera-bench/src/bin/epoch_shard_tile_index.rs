//! Epoch shards and the tile index: what N shards cost the hierarchy and the candidacy walk.
//!
//! An epoch shard owns a `u32` entity space and a `u32` Morton-ordered row space of its own.
//! Points are assigned to a shard by allocation time, so a spatial cluster has members in every
//! shard, the tile index holds one entry per (shard, ordinal), and candidacy runs once per shard
//! per request. This bin measures, on a real bundle, for N in {1, 2, 4, 8}:
//!
//! - the index bytes summed over shards, against N = 1;
//! - the ordinals with a `Span` extent per shard, which says how many artifacts touch a shard;
//! - the candidacy walk (`TileIndex::candidates`, then `TileIndex::inside` for every candidate
//!   the walk did not settle, which is `ArtifactRows::candidate_in`'s prefix) summed over shards
//!   at viewports of depth 0, 4, 8 and 12, several tiles per depth;
//! - the histogram walk (`RowColumn::histogram_over`) summed over shards, with `--histogram`.
//!
//! Nothing in the engine changes. A shard is a real `Permutation`, written with the shipped writer
//! over the shard's rows renumbered densely in their existing Morton order, and its index is
//! `TileIndex::project` over the level's records restricted to the shard's entities. At N = 1 the
//! shard is the bundle's own row space: the written permutation is compared byte for byte with
//! the bundle's, the index is compared with a fold-written one where the manifest lists one, and
//! `--verify-build` compares it with `TileIndex::build` over `MembershipRows`.
//!
//! Two shard rules run because they bracket the answer. `contiguous` takes equal ranges of entity
//! id. A build assigns ids in term-signature order, so the members of a term-defined artifact sit
//! close in id space, and this is the optimistic model. `hash` assigns by a mix of the id and is
//! the null model for epochs whose contents are uncorrelated with any artifact.
//!
//! Bytes are the extent column (`TileIndex::as_bytes`) plus the Portable serialised size of every
//! node bitmap and of the `everywhere` set. The engine keeps its node maps private, so this bin
//! places each extent by the engine's rule (`tile_index_shifts`, the finest level whose block
//! holds both ends of the span) and checks its `everywhere` count against the engine's.
//!
//! Residency: one shard's restricted records and permutation are held at a time and dropped once
//! its index (and column, with `--histogram`) is built. What the candidacy timing keeps per shard
//! is its row list and its index.
//!
//! Run:
//! `cargo run --release -p tessera-bench --bin epoch_shard_tile_index -- --fixture <root>
//! --layer <name> --scratch <dir>`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use croaring::{Bitmap, Portable};
use rustc_hash::FxHashMap;
use serde::Serialize;

use tessera_engine::artifacts::MembershipRows;
use tessera_engine::row_column::RowColumn;
use tessera_engine::tile_index::{Extent, TileIndex};
use tessera_lifecycle::membership::{decode_record, ArtifactRecord, Members};
use tessera_store::derived::tile_index_shifts;
use tessera_store::manifest::SegmentsManifest;
use tessera_store::membership::{row_column_width, MembershipPack};
use tessera_store::permutation::{Permutation, RowSpace};
use tessera_store::read::MortonSlice;
use tessera_store::write::write_permutation_iter;
use tessera_types::layer::ServingLayout;
use tessera_types::EntityId;

#[derive(Parser)]
#[command(about = "Tile index size and candidacy cost under N epoch shards, on a real bundle")]
struct Args {
    /// Bundle root (the directory containing `CURRENT`). Opened read-only.
    #[arg(long)]
    fixture: PathBuf,
    /// Layer name, as the segments manifest registers it.
    #[arg(long)]
    layer: String,
    #[arg(long, default_value_t = 0)]
    level: u32,
    /// Shard counts to run.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8")]
    shards: Vec<u32>,
    /// Shard rules: `contiguous`, `hash`.
    #[arg(long, value_delimiter = ',', default_value = "contiguous,hash")]
    rules: Vec<String>,
    /// Viewport depths. A viewport is one tile's row range.
    #[arg(long, value_delimiter = ',', default_value = "0,4,8,12")]
    depths: Vec<u8>,
    /// Tiles per depth, each the tile holding a row drawn at random from the row space.
    #[arg(long, default_value_t = 8)]
    tiles_per_depth: usize,
    /// Repetitions per timed cell; the median is taken.
    #[arg(long, default_value_t = 5)]
    reps: usize,
    /// Compose a row-major list column per shard and time its histogram walk.
    #[arg(long)]
    histogram: bool,
    /// A fold-written list column to use for the N = 1 histogram, where composing one would not
    /// fit the memory budget.
    #[arg(long)]
    n1_row_column: Option<PathBuf>,
    /// A column whose composition is modelled to push the resident set past this is skipped.
    #[arg(long, default_value_t = 7.0)]
    histogram_budget_gb: f64,
    /// At N = 1, also build `MembershipRows` and `TileIndex::build` and compare the bytes.
    #[arg(long)]
    verify_build: bool,
    /// Directory for the shard permutation files. One is held at a time and removed after use.
    #[arg(long)]
    scratch: PathBuf,
    /// Where to write the raw numbers.
    #[arg(long)]
    json: Option<PathBuf>,
    #[arg(long, default_value_t = 0x5EED)]
    seed: u64,
}

// ---------------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------------

struct Fixture {
    prefix_dir: PathBuf,
    manifest: SegmentsManifest,
    view: String,
    row_count: u32,
    bound: u64,
    permutation_path: PathBuf,
    /// `entity_of_row[row]`, from `row-entity.u32`.
    entity_of_row: Vec<u32>,
    morton: MortonSlice,
    ordinals: u32,
    /// Live ordinals with their records. A hole (an empty blob) is absent.
    records: Vec<(u32, ArtifactRecord)>,
    memberships: u64,
    undecodable: u32,
}

fn open_fixture(args: &Args) -> Fixture {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(args.fixture.join("CURRENT")).expect("read CURRENT"))
            .expect("CURRENT parses");
    let prefix = current["prefix"].as_str().expect("CURRENT.prefix");
    let prefix_dir = args.fixture.join(prefix);
    let partition_dir = prefix_dir.join("partitions").join("default");

    // The highest SEGMENTS-<n>.json present. The store walks down from it verifying digests;
    // this bin reads the bundle only and takes the newest.
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&partition_dir).expect("partition dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(n) = name
            .strip_prefix("SEGMENTS-")
            .and_then(|s| s.strip_suffix(".json"))
            .and_then(|s| s.parse::<u64>().ok())
        {
            if best.as_ref().is_none_or(|(b, _)| n > *b) {
                best = Some((n, entry.path()));
            }
        }
    }
    let (segments_n, segments_path) = best.expect("a SEGMENTS-<n>.json");
    let manifest: SegmentsManifest =
        serde_json::from_slice(&std::fs::read(&segments_path).expect("read segments manifest"))
            .expect("segments manifest parses");

    let layer = manifest
        .layers
        .iter()
        .find(|l| l.declaration.name == args.layer)
        .unwrap_or_else(|| panic!("layer {} is not registered", args.layer));
    let view = layer
        .declaration
        .views
        .first()
        .expect("the layer names a view")
        .clone();
    let segments: Vec<_> = manifest
        .segments
        .iter()
        .filter(|s| s.view == view)
        .collect();
    assert_eq!(
        segments.len(),
        1,
        "one build segment per view is what the shard row space is defined over; view {view} has {}",
        segments.len()
    );
    let row_count = segments[0].row_count;
    let view_dir = partition_dir.join("views").join(&view);
    let permutation_path = view_dir.join("permutation.bin");
    // Loaded for its bound only. The shard permutations below are written over the same bound,
    // and at N = 1 the written file is compared with this one byte for byte.
    let bound = Permutation::load(&permutation_path)
        .expect("permutation loads")
        .bound();
    let entity_of_row = {
        let bytes = std::fs::read(view_dir.join("row-entity.u32")).expect("row-entity.u32");
        assert_eq!(
            bytes.len(),
            row_count as usize * 4,
            "row-entity.u32 covers the segment"
        );
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect::<Vec<u32>>()
    };
    let morton = MortonSlice::load(
        &view_dir
            .join("segments")
            .join(&segments[0].seg_id)
            .join("morton.u32"),
    )
    .expect("morton.u32 loads");
    assert_eq!(morton.len(), row_count as usize);

    let extents: Vec<_> = manifest
        .membership_extents
        .iter()
        .filter(|e| e.layer == args.layer && e.level == args.level)
        .collect();
    assert!(!extents.is_empty(), "no membership extent for the level");
    let ordinals = extents
        .iter()
        .map(|e| e.ordinal_lo + e.count)
        .max()
        .unwrap_or(0);

    let started = Instant::now();
    let mut records = Vec::new();
    let mut memberships = 0u64;
    let mut undecodable = 0u32;
    for extent in &extents {
        let pack =
            MembershipPack::open(&prefix_dir.join(&extent.path)).expect("membership pack opens");
        assert_eq!(pack.ordinal_lo(), extent.ordinal_lo);
        assert_eq!(pack.count(), extent.count);
        for (ordinal, blob) in pack.iter() {
            if blob.is_empty() {
                continue;
            }
            // The artifact's own entity is not used by anything below; the members are.
            match decode_record(EntityId::new(0), blob) {
                Some((record, _)) => {
                    memberships += record.members.cardinality();
                    records.push((ordinal, record));
                }
                None => undecodable += 1,
            }
        }
    }
    records.sort_by_key(|(o, _)| *o);
    println!(
        "fixture {} (SEGMENTS-{segments_n}), layer {} level {}, view {view}: {row_count} rows, \
         bound {bound}, {ordinals} ordinals, {} live, {memberships} memberships, {undecodable} \
         undecodable; records decoded in {:.1} s, resident {:.2} GB",
        args.fixture.display(),
        args.layer,
        args.level,
        records.len(),
        started.elapsed().as_secs_f64(),
        gb(resident_bytes())
    );

    Fixture {
        prefix_dir,
        manifest,
        view,
        row_count,
        bound,
        permutation_path,
        entity_of_row,
        morton,
        ordinals,
        records,
        memberships,
        undecodable,
    }
}

// ---------------------------------------------------------------------------------------------
// Shards
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rule {
    Contiguous,
    Hash,
}

impl Rule {
    fn parse(word: &str) -> Rule {
        match word {
            "contiguous" => Rule::Contiguous,
            "hash" => Rule::Hash,
            other => panic!("unknown rule {other}; use contiguous or hash"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Rule::Contiguous => "contiguous",
            Rule::Hash => "hash",
        }
    }
}

/// splitmix64's finaliser: a fixed mix of the entity id, so a run is reproducible.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Sharding {
    rule: Rule,
    n: u32,
    /// Entities per shard under the contiguous rule.
    chunk: u64,
}

impl Sharding {
    fn new(rule: Rule, n: u32, bound: u64) -> Self {
        Sharding {
            rule,
            n,
            chunk: bound.div_ceil(n as u64).max(1),
        }
    }

    fn shard_of(&self, entity: u64) -> u32 {
        match self.rule {
            Rule::Contiguous => ((entity / self.chunk) as u32).min(self.n - 1),
            Rule::Hash => (mix(entity) % self.n as u64) as u32,
        }
    }

    fn entities_of(&self, shard: u32, bound: u64) -> Bitmap {
        match self.rule {
            Rule::Contiguous => {
                let lo = (shard as u64 * self.chunk).min(bound);
                let hi = if shard + 1 == self.n {
                    bound
                } else {
                    ((shard as u64 + 1) * self.chunk).min(bound)
                };
                Bitmap::from_range(lo as u32..hi as u32)
            }
            Rule::Hash => {
                let mut out = Bitmap::new();
                for e in 0..bound {
                    if self.shard_of(e) == shard {
                        out.add(e as u32);
                    }
                }
                out.run_optimize();
                out
            }
        }
    }
}

/// A shard permutation file, removed when the shard's build is dropped.
struct PermutationFile(PathBuf);

impl Drop for PermutationFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One shard's row space and the level restricted to it. Held while the shard's index and column
/// are built, then dropped.
struct ShardBuild {
    /// Full-space rows in this shard, ascending; the index is the shard-local row.
    rows: Vec<u32>,
    space: RowSpace,
    file: PermutationFile,
    /// The level's records with their members restricted to this shard's entities. Empty at
    /// N = 1, where the fixture's own records are used.
    restricted: Vec<(u32, ArtifactRecord)>,
    memberships: u64,
}

impl ShardBuild {
    fn records<'a>(&'a self, fixture: &'a Fixture) -> &'a [(u32, ArtifactRecord)] {
        if self.restricted.is_empty() && self.rows.len() == fixture.row_count as usize {
            &fixture.records
        } else {
            &self.restricted
        }
    }
}

/// What the candidacy timing keeps per shard.
struct ShardIndex {
    rows: Vec<u32>,
    index: TileIndex,
    memberships: u64,
}

impl ShardIndex {
    /// The shard-local row range of a full-space row range.
    fn local_range(&self, lo: u32, hi: u32) -> (u32, u32) {
        let a = self.rows.partition_point(|&r| r < lo) as u32;
        let b = self.rows.partition_point(|&r| r < hi) as u32;
        (a, b)
    }
}

fn build_shard(fixture: &Fixture, sharding: &Sharding, shard: u32, scratch: &Path) -> ShardBuild {
    let rows: Vec<u32> = (0..fixture.row_count)
        .filter(|&r| sharding.shard_of(fixture.entity_of_row[r as usize] as u64) == shard)
        .collect();
    let path = scratch.join(format!(
        "shard-{}-{}-{}.permutation.bin",
        sharding.rule.name(),
        sharding.n,
        shard
    ));
    let entity_of_row = &fixture.entity_of_row;
    write_permutation_iter(
        &path,
        rows.iter()
            .map(|&r| EntityId::new(entity_of_row[r as usize] as u64)),
        fixture.bound,
    )
    .expect("shard permutation writes");
    let file = PermutationFile(path);
    let permutation = Permutation::load(&file.0).expect("shard permutation loads");
    let space = RowSpace::new(Arc::new(permutation), rows.len() as u32);

    let whole = sharding.n == 1;
    let mut restricted = Vec::new();
    let mut memberships = fixture.memberships;
    if !whole {
        let entities = sharding.entities_of(shard, fixture.bound);
        memberships = 0;
        for (ordinal, record) in &fixture.records {
            let mut members = record.members.and(&entities);
            members.run_optimize();
            memberships += members.cardinality();
            restricted.push((
                *ordinal,
                ArtifactRecord {
                    entity: record.entity,
                    key: None,
                    view: None,
                    members: Members::owned(members),
                    contents: Vec::new(),
                    attached_to: None,
                    parents: Vec::new(),
                },
            ));
        }
    }
    ShardBuild {
        rows,
        space,
        file,
        restricted,
        memberships,
    }
}

fn project_index(fixture: &Fixture, shard: &ShardBuild) -> TileIndex {
    let records = shard.records(fixture);
    TileIndex::project(
        fixture.ordinals,
        || records.iter().map(|(o, r)| (*o, r)),
        &shard.space,
    )
}

// ---------------------------------------------------------------------------------------------
// What an index weighs
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone, Default)]
struct IndexShape {
    rows: u32,
    extent_bytes: u64,
    /// Portable serialised bytes of every `own` and `subtree` node bitmap and of `everywhere`.
    bitmap_bytes: u64,
    own_nodes: u64,
    subtree_nodes: u64,
    levels: usize,
    everywhere: u64,
    span: u64,
    empty: u64,
    hole: u64,
}

/// The hierarchy the engine folds over the extents, derived again here so it can be weighed.
/// The placement rule is `TileIndex::from_pack`'s; the `everywhere` count is asserted equal to the
/// engine's.
fn shape_of(index: &TileIndex) -> IndexShape {
    let shifts = tile_index_shifts(index.row_count());
    let mut own: Vec<FxHashMap<u32, Bitmap>> =
        (0..shifts.len()).map(|_| FxHashMap::default()).collect();
    let mut everywhere = Bitmap::new();
    let mut shape = IndexShape {
        rows: index.row_count(),
        extent_bytes: index.as_bytes().len() as u64,
        levels: shifts.len(),
        ..IndexShape::default()
    };
    for ordinal in 0..index.len() as u32 {
        match index.extent(ordinal) {
            Extent::Hole => shape.hole += 1,
            Extent::Empty => shape.empty += 1,
            Extent::Span { lo, hi } => {
                shape.span += 1;
                let placed = shifts
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, &s)| lo >> s == hi >> s);
                match placed {
                    Some((level, &s)) => {
                        own[level].entry(lo >> s).or_default().add(ordinal);
                    }
                    None => everywhere.add(ordinal),
                }
            }
        }
    }
    let mut subtree = own.clone();
    for level in (1..shifts.len()).rev() {
        let step = shifts[level - 1] - shifts[level];
        let (upper, lower) = subtree.split_at_mut(level);
        for (block, child) in lower[0].iter() {
            upper[level - 1]
                .entry(block >> step)
                .or_default()
                .or_inplace(child);
        }
    }
    for level in own.iter_mut().chain(subtree.iter_mut()) {
        for node in level.values_mut() {
            node.run_optimize();
        }
    }
    everywhere.run_optimize();
    assert_eq!(
        everywhere.cardinality(),
        index.everywhere(),
        "the re-derived placement disagrees with the engine about the everywhere set"
    );
    shape.everywhere = everywhere.cardinality();
    shape.own_nodes = own.iter().map(|l| l.len() as u64).sum();
    shape.subtree_nodes = subtree.iter().map(|l| l.len() as u64).sum();
    shape.bitmap_bytes = own
        .iter()
        .chain(subtree.iter())
        .flat_map(|l| l.values())
        .map(|b| b.get_serialized_size_in_bytes::<Portable>() as u64)
        .sum::<u64>()
        + everywhere.get_serialized_size_in_bytes::<Portable>() as u64;
    shape
}

// ---------------------------------------------------------------------------------------------
// Viewports
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct TileRange {
    depth: u8,
    prefix: u64,
    /// Full-space rows `[lo, hi)`.
    lo: u32,
    hi: u32,
}

fn pick_tiles(codes: &[u32], depths: &[u8], per_depth: usize, seed: u64) -> Vec<TileRange> {
    let mut tiles = Vec::new();
    let mut state = seed;
    for &depth in depths {
        if depth == 0 {
            tiles.push(TileRange {
                depth,
                prefix: 0,
                lo: 0,
                hi: codes.len() as u32,
            });
            continue;
        }
        let shift = 32 - 2 * depth as u32;
        let mut seen = Vec::new();
        // Draws until the quota is met, bounded so a corpus with few distinct tiles at a depth
        // does not loop forever.
        for _ in 0..per_depth * 64 {
            if seen.len() >= per_depth {
                break;
            }
            state = mix(state);
            let row = (state % codes.len() as u64) as usize;
            let prefix = (codes[row] as u64) >> shift;
            if seen.contains(&prefix) {
                continue;
            }
            seen.push(prefix);
            let lo = codes.partition_point(|&c| (c as u64) < (prefix << shift));
            let hi = codes.partition_point(|&c| (c as u64) < ((prefix + 1) << shift));
            tiles.push(TileRange {
                depth,
                prefix,
                lo: lo as u32,
                hi: hi as u32,
            });
        }
    }
    tiles
}

#[derive(Serialize, Clone)]
struct CandidacySample {
    depth: u8,
    prefix: u64,
    full_rows: u32,
    /// Median over reps of the walk plus the extent test, summed over shards, in microseconds.
    micros: f64,
    candidates: u64,
    settled: u64,
    inside: u64,
    nodes_visited: u64,
    /// Shards whose local viewport held no rows.
    empty_shards: u32,
}

fn time_candidacy(shards: &[ShardIndex], tile: &TileRange, reps: usize) -> CandidacySample {
    let mut samples = Vec::with_capacity(reps);
    let mut candidates = 0u64;
    let mut settled = 0u64;
    let mut inside = 0u64;
    let mut nodes = 0u64;
    let mut empty_shards = 0u32;
    for rep in 0..reps {
        let mut total = 0f64;
        for shard in shards {
            let (lo, hi) = shard.local_range(tile.lo, tile.hi);
            let viewport = Bitmap::from_range(lo..hi);
            let started = Instant::now();
            let found = shard.index.candidates(&viewport);
            let viewport_rows = viewport.cardinality();
            let mut inside_here = 0u64;
            for ordinal in found.iter() {
                if !found.is_settled(ordinal)
                    && shard.index.inside(ordinal, &viewport, viewport_rows)
                {
                    inside_here += 1;
                }
            }
            total += started.elapsed().as_secs_f64() * 1e6;
            if rep == 0 {
                candidates += found.len();
                settled += found.settled_len();
                inside += inside_here;
                nodes += found.nodes_visited();
                if lo == hi {
                    empty_shards += 1;
                }
            }
        }
        samples.push(total);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    CandidacySample {
        depth: tile.depth,
        prefix: tile.prefix,
        full_rows: tile.hi - tile.lo,
        micros: samples[samples.len() / 2],
        candidates,
        settled,
        inside,
        nodes_visited: nodes,
        empty_shards,
    }
}

// ---------------------------------------------------------------------------------------------
// The histogram walk
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Clone, Default)]
struct HistogramResult {
    /// One entry per shard: how the column was obtained.
    source: Vec<String>,
    /// Median over reps of the walk, summed over shards, in milliseconds.
    millis: f64,
    entries: u64,
    column_bytes: u64,
    skipped: Option<String>,
    rayon_threads: usize,
}

fn time_histogram(column: &RowColumn, rows: u32, reps: usize) -> (f64, u64) {
    let mask = Bitmap::from_range(0..rows);
    let mut samples = Vec::with_capacity(reps);
    let mut total = 0u64;
    for rep in 0..reps {
        let started = Instant::now();
        let counts = column.histogram_over(&mask);
        samples.push(started.elapsed().as_secs_f64() * 1e3);
        if rep == 0 {
            total = counts.iter().map(|&c| c as u64).sum();
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    (samples[samples.len() / 2], total)
}

/// Compose (or open) one shard's list column and time `histogram_over` on the whole shard,
/// adding to `result`. Returns false where the column was skipped for memory.
fn histogram_shard(
    args: &Args,
    fixture: &Fixture,
    shard: &ShardBuild,
    n: u32,
    s: u32,
    result: &mut HistogramResult,
) -> bool {
    let rows = shard.rows.len() as u32;
    let width = u64::from(row_column_width(u64::from(fixture.ordinals)));
    let budget = (args.histogram_budget_gb * 1e9) as u64;
    let opened = if n == 1 {
        args.n1_row_column.as_ref().map(|path| {
            let column = RowColumn::open(path, ServingLayout::RowMajorList)
                .expect("the supplied list column opens");
            assert_eq!(
                column.row_count(),
                rows,
                "the supplied column covers the row space"
            );
            assert_eq!(column.len(), fixture.ordinals as usize);
            (column, format!("opened {}", path.display()))
        })
    } else {
        None
    };
    let (column, source) = match opened {
        Some(pair) => pair,
        None => {
            // `project_row_column` holds one u32 per entry, one u32 per row twice over, and the
            // packed column, before the transient is dropped.
            let entries = shard.memberships;
            let transient = entries * 4 + rows as u64 * 8 + entries * width;
            let resident = resident_bytes();
            if resident + transient > budget {
                let note = format!(
                    "shard {s}: composing the column needs {:.2} GB over {:.2} GB resident, above \
                     the {:.1} GB budget; skipped",
                    gb(transient),
                    gb(resident),
                    args.histogram_budget_gb
                );
                println!("  histogram: {note}");
                result.skipped = Some(note);
                return false;
            }
            let records = shard.records(fixture);
            let started = Instant::now();
            let column = RowColumn::project(
                fixture.ordinals,
                &shard.space,
                ServingLayout::RowMajorList,
                || records.iter().map(|(o, r)| (*o, r)),
            )
            .expect("a list column always composes");
            (
                column,
                format!("composed in {:.1} s", started.elapsed().as_secs_f64()),
            )
        }
    };
    let (millis, entries) = time_histogram(&column, rows, args.reps);
    println!(
        "  histogram shard {s}: {source}; {} entries, column {:.1} MB, walk {:.1} ms (resident \
         {:.2} GB)",
        entries,
        mb(column.as_bytes().len() as u64),
        millis,
        gb(resident_bytes())
    );
    result.source.push(source);
    result.millis += millis;
    result.entries += entries;
    result.column_bytes += column.as_bytes().len() as u64;
    true
}

// ---------------------------------------------------------------------------------------------
// Process accounting
// ---------------------------------------------------------------------------------------------

/// This process's resident set, from `/proc/self/statm`.
fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("/proc/self/statm");
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .expect("resident pages");
    // SAFETY: `sysconf` is a pure lookup with no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    pages * page
}

/// The process's peak resident set, from `VmHWM` in `/proc/self/status`.
fn peak_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct Checks {
    /// The N = 1 shard permutation against the bundle's `permutation.bin`, byte for byte.
    permutation_identical: Option<bool>,
    /// The N = 1 projected index against a fold-written one the manifest lists, byte for byte,
    /// with the two level versions.
    fold_written: Option<FoldWrittenCheck>,
    /// The N = 1 projected index against `TileIndex::build` over `MembershipRows`.
    build_identical: Option<bool>,
}

#[derive(Serialize)]
struct FoldWrittenCheck {
    path: String,
    file_level_version: u64,
    level_version: Option<u64>,
    bytes_identical: bool,
    ordinals_equal: bool,
}

#[derive(Serialize)]
struct DepthSummary {
    depth: u8,
    tiles: usize,
    mean_micros: f64,
    mean_candidates: f64,
    mean_settled: f64,
    mean_inside: f64,
    mean_nodes_visited: f64,
}

#[derive(Serialize)]
struct Run {
    rule: &'static str,
    shards: u32,
    per_shard: Vec<IndexShape>,
    per_shard_rows: Vec<u32>,
    per_shard_memberships: Vec<u64>,
    total_bytes: u64,
    extent_bytes: u64,
    bitmap_bytes: u64,
    span_sum: u64,
    /// Ordinals with a `Span` extent in every shard.
    span_in_all: u64,
    build_seconds: f64,
    candidacy: Vec<CandidacySample>,
    by_depth: Vec<DepthSummary>,
    histogram: Option<HistogramResult>,
    resident_after_gb: f64,
}

#[derive(Serialize)]
struct Report {
    fixture: String,
    layer: String,
    level: u32,
    view: String,
    row_count: u32,
    bound: u64,
    ordinals: u32,
    live: usize,
    memberships: u64,
    undecodable: u32,
    reps: usize,
    tiles: Vec<TileRange>,
    checks: Checks,
    runs: Vec<Run>,
    peak_resident_gb: f64,
}

fn summarise(samples: &[CandidacySample], depths: &[u8]) -> Vec<DepthSummary> {
    depths
        .iter()
        .map(|&depth| {
            let at: Vec<&CandidacySample> = samples.iter().filter(|s| s.depth == depth).collect();
            let n = at.len().max(1) as f64;
            let mean =
                |f: &dyn Fn(&CandidacySample) -> f64| at.iter().map(|s| f(s)).sum::<f64>() / n;
            DepthSummary {
                depth,
                tiles: at.len(),
                mean_micros: mean(&|s| s.micros),
                mean_candidates: mean(&|s| s.candidates as f64),
                mean_settled: mean(&|s| s.settled as f64),
                mean_inside: mean(&|s| s.inside as f64),
                mean_nodes_visited: mean(&|s| s.nodes_visited as f64),
            }
        })
        .collect()
}

/// The N = 1 checks: the written permutation, a fold-written index, and `TileIndex::build`.
fn check_whole(args: &Args, fixture: &Fixture, shard: &ShardBuild, index: &TileIndex) -> Checks {
    let mine = std::fs::read(&shard.file.0).expect("read shard permutation");
    let theirs = std::fs::read(&fixture.permutation_path).expect("read bundle permutation");
    let identical = mine == theirs;
    println!(
        "  check: N=1 permutation {} the bundle's permutation.bin ({} against {} bytes)",
        if identical {
            "is identical to"
        } else {
            "DIFFERS from"
        },
        mine.len(),
        theirs.len()
    );
    let mut checks = Checks {
        permutation_identical: Some(identical),
        fold_written: None,
        build_identical: None,
    };

    let level_version = fixture
        .manifest
        .level_versions
        .iter()
        .find(|v| v.layer == args.layer && v.level == args.level)
        .map(|v| v.version);
    if let Some(extent) = fixture
        .manifest
        .tile_index_extents
        .iter()
        .find(|e| e.layer == args.layer && e.level == args.level && e.view == fixture.view)
    {
        let written = TileIndex::open(&fixture.prefix_dir.join(&extent.path))
            .expect("fold-written index opens");
        let bytes_identical = written.as_bytes() == index.as_bytes();
        let ordinals_equal = written.len() == index.len();
        println!(
            "  check: fold-written index {} (level version {} in the manifest, level now at \
             {:?}): bytes {}, ordinals {} vs {}",
            extent.path,
            extent.level_version,
            level_version,
            if bytes_identical {
                "identical"
            } else {
                "DIFFER"
            },
            written.len(),
            index.len()
        );
        checks.fold_written = Some(FoldWrittenCheck {
            path: extent.path.clone(),
            file_level_version: extent.level_version,
            level_version,
            bytes_identical,
            ordinals_equal,
        });
    } else {
        println!("  check: the manifest lists no fold-written tile index for this level");
    }

    if args.verify_build {
        let started = Instant::now();
        let membership =
            MembershipRows::build(fixture.records.iter().map(|(o, r)| (*o, r)), &shard.space);
        let built = TileIndex::build(&membership, fixture.row_count);
        let identical = built.as_bytes() == index.as_bytes();
        println!(
            "  check: TileIndex::build over MembershipRows {} the projected index ({:.1} s, \
             resident {:.2} GB)",
            if identical {
                "is identical to"
            } else {
                "DIFFERS from"
            },
            started.elapsed().as_secs_f64(),
            gb(resident_bytes())
        );
        checks.build_identical = Some(identical);
    }
    checks
}

fn main() {
    let args = Args::parse();
    std::fs::create_dir_all(&args.scratch).expect("scratch dir");
    let rules: Vec<Rule> = args.rules.iter().map(|r| Rule::parse(r)).collect();
    let fixture = open_fixture(&args);
    let tiles = pick_tiles(
        fixture.morton.u32(),
        &args.depths,
        args.tiles_per_depth,
        args.seed,
    );
    println!("\nviewports:");
    for t in &tiles {
        println!(
            "  depth {:>2} prefix {:>10} rows [{}, {}) = {} rows",
            t.depth,
            t.prefix,
            t.lo,
            t.hi,
            t.hi - t.lo
        );
    }

    let mut checks = Checks {
        permutation_identical: None,
        fold_written: None,
        build_identical: None,
    };
    let mut runs: Vec<Run> = Vec::new();
    let mut baseline: Option<(u64, u64, Vec<DepthSummary>, Option<f64>)> = None;

    for &rule in &rules {
        for &n in &args.shards {
            if n == 1 && baseline.is_some() {
                // N = 1 is the same shard under either rule.
                continue;
            }
            let sharding = Sharding::new(rule, n, fixture.bound);
            println!(
                "\n== rule {} shards {} (resident {:.2} GB) ==",
                rule.name(),
                n,
                gb(resident_bytes())
            );
            let started = Instant::now();
            let mut shards: Vec<ShardIndex> = Vec::with_capacity(n as usize);
            let mut per_shard = Vec::new();
            let mut span_in_all: Option<Bitmap> = None;
            let mut histogram = args.histogram.then(|| HistogramResult {
                rayon_threads: rayon::current_num_threads(),
                ..HistogramResult::default()
            });
            for s in 0..n {
                let build = build_shard(&fixture, &sharding, s, &args.scratch);
                let index = project_index(&fixture, &build);
                let shape = shape_of(&index);
                let spans: Bitmap = (0..index.len() as u32)
                    .filter(|&o| matches!(index.extent(o), Extent::Span { .. }))
                    .collect();
                span_in_all = Some(match span_in_all {
                    None => spans,
                    Some(acc) => acc.and(&spans),
                });
                println!(
                    "  shard {s}: {} rows, {} memberships, span {} empty {} hole {}, everywhere {}, \
                     own nodes {} subtree nodes {}, extents {:.3} MB bitmaps {:.3} MB (resident \
                     {:.2} GB)",
                    build.rows.len(),
                    build.memberships,
                    shape.span,
                    shape.empty,
                    shape.hole,
                    shape.everywhere,
                    shape.own_nodes,
                    shape.subtree_nodes,
                    mb(shape.extent_bytes),
                    mb(shape.bitmap_bytes),
                    gb(resident_bytes())
                );
                per_shard.push(shape);
                if n == 1 {
                    checks = check_whole(&args, &fixture, &build, &index);
                }
                if let Some(result) = histogram.as_mut() {
                    if result.skipped.is_none() {
                        histogram_shard(&args, &fixture, &build, n, s, result);
                    }
                }
                let ShardBuild {
                    rows, memberships, ..
                } = build;
                shards.push(ShardIndex {
                    rows,
                    index,
                    memberships,
                });
            }
            let build_seconds = started.elapsed().as_secs_f64();
            if let Some(result) = &histogram {
                if result.skipped.is_none() {
                    println!(
                        "  histogram summed over {n} shards: {:.1} ms, {} entries, {} rayon threads",
                        result.millis, result.entries, result.rayon_threads
                    );
                }
            }

            // Candidacy, summed over shards, per tile.
            let candidacy: Vec<CandidacySample> = tiles
                .iter()
                .map(|t| time_candidacy(&shards, t, args.reps))
                .collect();
            let by_depth = summarise(&candidacy, &args.depths);
            println!(
                "  {:>5} {:>5} {:>12} {:>12} {:>10} {:>10} {:>10}",
                "depth", "tiles", "µs (sum)", "candidates", "settled", "inside", "nodes"
            );
            for d in &by_depth {
                println!(
                    "  {:>5} {:>5} {:>12.1} {:>12.1} {:>10.1} {:>10.1} {:>10.1}",
                    d.depth,
                    d.tiles,
                    d.mean_micros,
                    d.mean_candidates,
                    d.mean_settled,
                    d.mean_inside,
                    d.mean_nodes_visited
                );
            }

            let extent_bytes: u64 = per_shard.iter().map(|s| s.extent_bytes).sum();
            let bitmap_bytes: u64 = per_shard.iter().map(|s| s.bitmap_bytes).sum();
            let span_sum: u64 = per_shard.iter().map(|s| s.span).sum();
            let run = Run {
                rule: rule.name(),
                shards: n,
                per_shard_rows: shards.iter().map(|s| s.rows.len() as u32).collect(),
                per_shard_memberships: shards.iter().map(|s| s.memberships).collect(),
                per_shard,
                total_bytes: extent_bytes + bitmap_bytes,
                extent_bytes,
                bitmap_bytes,
                span_sum,
                span_in_all: span_in_all.map(|b| b.cardinality()).unwrap_or(0),
                build_seconds,
                candidacy,
                by_depth,
                histogram,
                resident_after_gb: gb(resident_bytes()),
            };
            if n == 1 {
                baseline = Some((
                    run.total_bytes,
                    run.span_sum,
                    summarise(&run.candidacy, &args.depths),
                    run.histogram
                        .as_ref()
                        .and_then(|h| h.skipped.is_none().then_some(h.millis)),
                ));
            }
            if let Some((bytes1, span1, depths1, hist1)) = &baseline {
                let depth_ratios: Vec<String> = run
                    .by_depth
                    .iter()
                    .zip(depths1.iter())
                    .map(|(d, d1)| format!("d{} {:.2}×", d.depth, d.mean_micros / d1.mean_micros))
                    .collect();
                println!(
                    "  against N=1: bytes {:.2}×, span ordinals {:.2}×, candidacy {}{}",
                    run.total_bytes as f64 / *bytes1 as f64,
                    run.span_sum as f64 / *span1 as f64,
                    depth_ratios.join(" "),
                    match (hist1, run.histogram.as_ref()) {
                        (Some(h1), Some(h)) if h.skipped.is_none() =>
                            format!(", histogram {:.2}×", h.millis / h1),
                        _ => String::new(),
                    }
                );
            }
            println!(
                "  built in {:.1} s; total index {:.3} MB (extents {:.3} MB, bitmaps {:.3} MB); \
                 span ordinals summed {}, in every shard {}; resident {:.2} GB",
                build_seconds,
                mb(run.total_bytes),
                mb(run.extent_bytes),
                mb(run.bitmap_bytes),
                run.span_sum,
                run.span_in_all,
                run.resident_after_gb
            );
            runs.push(run);
            drop(shards);
        }
    }

    let report = Report {
        fixture: args.fixture.display().to_string(),
        layer: args.layer.clone(),
        level: args.level,
        view: fixture.view.clone(),
        row_count: fixture.row_count,
        bound: fixture.bound,
        ordinals: fixture.ordinals,
        live: fixture.records.len(),
        memberships: fixture.memberships,
        undecodable: fixture.undecodable,
        reps: args.reps,
        tiles,
        checks,
        runs,
        peak_resident_gb: gb(peak_bytes()),
    };
    println!("\npeak resident {:.2} GB", report.peak_resident_gb);
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&report).expect("json"))
            .expect("write json");
        println!("wrote {}", path.display());
    }
}
