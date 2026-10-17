//! Shared fixtures for `mosaica-engine`'s viewport-side integration tests.
//!
//! The viewport cases and the pin cases live in separate binaries (`tests/viewport.rs` and
//! `tests/pins.rs`). This module holds the fixture corpus, the `EngineConfig`s and the credentials
//! both of them use.
//!
//! Every item carries `ALL_TERM` ("0"); every third item (`source_id % 3 == 0`) additionally
//! carries `SUBSET_TERM` ("1").

// Each integration-test binary compiles this module separately, so a fixture used by only one of
// them is genuinely dead code in the other. Allowing it here is what keeps the binaries from each
// carrying their own copy — which is the drift this module exists to prevent.
#![allow(dead_code)]

mod lifecycle;
mod shards;
mod wait;

pub use ingest_rows::IngestRows;
#[allow(unused_imports)]
pub use lifecycle::*;
#[allow(unused_imports)]
pub use shards::*;
#[allow(unused_imports)]
pub use wait::*;

mod ingest_rows {
    use mosaica_engine::{AcceptError, Engine, IngestRequest};
    use mosaica_lifecycle::{BatchArtifacts, IngestRow, UnallocatedRow};
    use mosaica_types::EntityId;

    /// Ingest through [`Engine::ingest`] from rows shaped as the executor writes them: each row
    /// names the item its `join` holds by `mosaica_id`, or none, carries its label and position,
    /// and carries every value it holds, a null clearing one. Every row names the same view.
    pub trait IngestRows {
        /// Send `rows` as one batch, and answer the entity each row created or named.
        fn ingest_rows(
            &self,
            rows: Vec<UnallocatedRow>,
            batch_id: String,
            body_hash: [u8; 32],
        ) -> Result<Vec<EntityId>, AcceptError> {
            self.ingest_rows_joining(rows, batch_id, body_hash, BatchArtifacts::default())
                .map(|(entities, _)| entities)
        }

        /// [`Self::ingest_rows`], with the artifacts the rows name in a layer's column, answering
        /// how many artifacts the batch minted beside the entities.
        fn ingest_rows_joining(
            &self,
            rows: Vec<UnallocatedRow>,
            batch_id: String,
            body_hash: [u8; 32],
            artifacts: BatchArtifacts,
        ) -> Result<(Vec<EntityId>, u64), AcceptError>;
    }

    impl IngestRows for Engine {
        fn ingest_rows_joining(
            &self,
            rows: Vec<UnallocatedRow>,
            batch_id: String,
            body_hash: [u8; 32],
            artifacts: BatchArtifacts,
        ) -> Result<(Vec<EntityId>, u64), AcceptError> {
            let view = rows.first().map(|row| row.view.clone());
            assert!(
                rows.iter().all(|row| Some(&row.view) == view.as_ref()),
                "a batch names one view"
            );
            let rows = rows
                .into_iter()
                .map(|row| IngestRow {
                    mosaica_id: row.join.map(|item| {
                        self.mosaica_id_of(item.entity)
                            .expect("a joined item has an id")
                    }),
                    labels: Some(row.descriptors),
                    position: Some((row.x, row.y)),
                    scalars: row.scalars,
                    scoped: row.scoped,
                    omitted: Vec::new(),
                })
                .collect();
            let receipt = self.ingest(IngestRequest {
                batch_id,
                body_hash,
                view,
                rows,
                artifacts,
                strict: true,
                mosaica_id_column: false,
            })?;
            let ids: Vec<_> = receipt.mosaica_ids.iter().flatten().copied().collect();
            let entities = super::items_of(self, &ids)
                .into_iter()
                .map(|entity| entity.expect("an accepted row names an item"))
                .collect();
            Ok((entities, receipt.minted))
        }
    }
}

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{Array, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use mosaica_build::{build, BuildArgs};
use mosaica_engine::filter::FilterExpr;
use mosaica_engine::{
    default_compute_threads, AggregateCaps, AggregateHead, AggregateSink, ArtifactOut, Engine,
    EngineConfig, ItemsRequest, PageEnd, RecordsCounts, RecordsHead, RecordsLimits, RecordsOrder,
    RecordsSink, Session, SinkResult, TableHead,
};
use mosaica_lifecycle::UnallocatedRow;
use mosaica_spatial::Bounds;
use mosaica_store::manifest::SegmentsManifest;
use mosaica_store::read::open_bundle;
use mosaica_types::{EntityId, IdentityKey, ItemAt, ItemHigh, MosaicaId, ShardId, Tenancy};

pub const N_ITEMS: u64 = 10_000;
pub const ALL_TERM: u64 = 0;
pub const SUBSET_TERM: u64 = 1;

/// The built bundle's partition, and its one shard.
pub const PARTITION: &str = "default";
pub const SHARD: mosaica_store::ShardLayout<'static> =
    mosaica_store::ShardLayout::new(PARTITION, ShardId::ZERO);

/// The item each of `ids` names, in its shard: `None` for one that names nothing, an artifact or
/// a layer.
pub fn items_at(engine: &Engine, ids: &[MosaicaId]) -> Vec<Option<ItemAt>> {
    engine
        .resolve_targets(ids)
        .unwrap()
        .into_iter()
        .map(|target| match target {
            Some(mosaica_types::Target::Item(shard, entity)) => Some(ItemAt::new(shard, entity)),
            _ => None,
        })
        .collect()
}

/// [`items_at`], each item's entity alone.
pub fn items_of(engine: &Engine, ids: &[MosaicaId]) -> Vec<Option<EntityId>> {
    let items = items_at(engine, ids).into_iter();
    items.map(|item| item.map(|item| item.entity)).collect()
}

/// The built bundle's partition directory in `root`'s `prefix`: where its side-manifests are.
pub fn partition_dir(root: &Path, prefix: &str) -> PathBuf {
    mosaica_store::layout::partition_dir(&root.join(prefix), PARTITION)
}

/// The shard's directory in the prefix `CURRENT` names.
pub fn shard_dir(root: &Path) -> PathBuf {
    SHARD.dir(&root.join(current_prefix(root)))
}

/// The side-manifest `root` serves, as an open reads it.
pub fn side_manifest(root: &Path) -> SegmentsManifest {
    open_bundle(root).expect("the bundle opens").partitions[PARTITION].side_manifest()
}

/// The name of the layer `manifest` registers in `space`.
pub fn layer_in(manifest: &SegmentsManifest, space: mosaica_types::LayerSpace) -> &str {
    let layer = manifest.partition.layers.iter().find(|l| l.space == space);
    &layer.expect("a registered layer holds the space").declaration.name
}

/// Write `manifest` as publication `n` of `prefix_dir`'s partition in place of the one there:
/// its partition file and every shard and space file numbered `n` are replaced.
pub fn rewrite_publication(prefix_dir: &Path, n: u64, manifest: &SegmentsManifest) {
    let refs = mosaica_store::read_publication(prefix_dir, PARTITION, n)
        .expect("the publication reads")
        .refs;
    let shards = refs.shards.iter().filter(|named| named.n == n);
    let spaces = refs.layer_spaces.iter().filter(|named| named.n == n);
    let written = shards
        .map(|named| mosaica_store::ShardLayout::new(PARTITION, named.id).manifest_rel(n))
        .chain(spaces.map(|named| {
            mosaica_store::LayerSpaceLayout::new(PARTITION, named.space).manifest_rel(n)
        }))
        .chain([format!(
            "{}/{}",
            mosaica_store::layout::partition_rel(PARTITION),
            mosaica_store::layout::segments_name(n)
        )]);
    for rel in written {
        std::fs::remove_file(prefix_dir.join(rel)).expect("the publication's file is removed");
    }
    mosaica_store::write_publication(prefix_dir, PARTITION, n, manifest, None)
        .expect("the publication is written again");
}

/// The number of the publication `root` serves, as its partition file names it.
pub fn served_n(root: &Path) -> u64 {
    open_bundle(root).expect("the bundle opens").partitions[PARTITION].segments_n
}

/// The publication `root` serves.
pub fn served_publication(root: &Path) -> mosaica_store::read::PublishedManifest {
    mosaica_store::read_publication(&root.join(current_prefix(root)), PARTITION, served_n(root))
        .expect("the served publication reads")
}

/// Rewrite the side-manifest `root` serves with `edit` applied.
pub fn edit_side_manifest(root: &Path, edit: impl FnOnce(&mut SegmentsManifest)) {
    let published = served_publication(root);
    let mut manifest = published.manifest;
    edit(&mut manifest);
    rewrite_publication(&root.join(current_prefix(root)), published.n, &manifest);
}

/// Every file under `dir` and its subdirectories, none where `dir` is missing.
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match path.is_dir() {
            true => out.extend(files_under(&path)),
            false => out.push(path),
        }
    }
    out
}

/// The bundle's shard size, and each shard's id and high water, as an open reads them.
pub fn shards_of(root: &Path) -> (u64, Vec<(ShardId, u64)>) {
    let bundle = open_bundle(root).expect("the bundle opens");
    let partition = &bundle.partitions[PARTITION];
    let shards = partition
        .shards
        .iter()
        .map(|shard| (shard.record.id, shard.record.high_water))
        .collect();
    (partition.shard_size, shards)
}

/// Every segment's directory in the prefix `CURRENT` names.
pub fn segment_dirs(root: &Path) -> Vec<PathBuf> {
    let bundle = open_bundle(root).expect("the bundle opens");
    let prefix = root.join(current_prefix(root));
    bundle
        .only_shard()
        .record
        .segments
        .iter()
        .map(|segment| prefix.join(SHARD.segment_rel(&segment.view, &segment.seg_id)))
        .collect()
}

/// The files of one derived `kind` in the shard of the prefix `engine` serves.
pub fn live_shard_files(root: &Path, engine: &Engine, kind: &str) -> Vec<PathBuf> {
    let dir = root
        .join(&engine.generation().prefix)
        .join(SHARD.derived_rel(kind));
    std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// `name` in the shard's directory for derived `kind`, prefix-relative, with that directory
/// created under `dir`.
pub fn derived_file(dir: &Path, kind: &str, name: &str) -> String {
    let kind_rel = SHARD.derived_rel(kind);
    std::fs::create_dir_all(dir.join(&kind_rel)).unwrap();
    format!("{kind_rel}/{name}")
}

/// A fixed, non-degenerate test key — the same canonical vector used across the identity
/// construction's own tests (`mosaica_types::identity`'s `CANONICAL_KEY`) and
/// `mosaica-build`'s fixture tests, so a mismatch between crates would show up as a vector
/// disagreement rather than an independently-chosen value.
pub const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

pub fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

/// The `mosaica_id` of the item of shard 0 holding `entity` as its number at `tenancy`.
pub fn mosaica_id_at(entity: EntityId, tenancy: u16) -> MosaicaId {
    let high = ItemHigh::new(
        ShardId::ZERO,
        Tenancy::new(tenancy).expect("a tenancy below 4,096"),
    );
    test_key().forward(high, entity).unwrap()
}

/// The tenancy `id` carries.
pub fn tenancy_of(id: MosaicaId) -> u16 {
    test_key()
        .invert(id)
        .expect("an item's mosaica_id inverts")
        .0
        .tenancy
        .raw()
}

pub fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// The base config for these tests.
///
/// **`theta_target_marks` is raised above `N_ITEMS` on purpose.** These tests assert *masking* —
/// which items a principal may see — not density. Leaving θ live would make every assertion about a
/// point set depend on the density rule's threshold clause as well, so a masking bug and a θ
/// arithmetic bug would be indistinguishable. Raising the target above the fixture's total visible
/// count saturates θ at every depth, which reduces selection to "serve every visible row up to the
/// cap" and isolates what these tests are for. Density itself is tested in `selection.rs`, against
/// fixtures built for it.
pub fn config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
        k_min: 2,
        k_max_marks: 200,
        theta_target_marks: N_ITEMS * 2,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads: default_compute_threads(),
        // The server's own defaults for lifecycle §2.2's two pin bounds, deliberately rather than
        // test-specific values: a case that means to exercise the TTL or the cap overrides the one
        // it is testing (`tests/pins.rs` does), so these must never be the reason a test passes.
        // A 300 s TTL means no case here reaches it by elapsing.
        flush_max_age_secs: 90,
        // The shipped row trigger, four commit windows (`DEFAULT_FLUSH_MAX_ITEMS`):
        // what bounds the window close's O(buffered) copy. Nothing here reaches it.
        flush_max_items: 40_000,
        // The built-in default; `tests/scale.rs` is where this knob is exercised.
        max_merged_segment_bytes: None,
        // The built-in merge and coalesce policies (width 4 / floor 16 MiB / width 8);
        // `tests/merge.rs` and `tests/coalesce.rs` are where the configured values are exercised.
        tier_width: None,
        segment_floor_bytes: None,
        coalesce_width: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: mosaica_engine::CompactionSchedule::off(),
    }
}

/// A config whose caps are large enough to never truncate a sample — used by tests asserting
/// membership (counts, exact point sets) rather than a cap itself. θ is saturated here too, for the
/// reason given on [`config`].
pub fn config_uncapped() -> EngineConfig {
    EngineConfig {
        max_k: N_ITEMS as usize,
        k_max_marks: N_ITEMS as usize,
        ..config()
    }
}

/// Every item carries `ALL_TERM`; every third carries `SUBSET_TERM` too.
pub fn terms_of(source_id: u64) -> Vec<u64> {
    if source_id.is_multiple_of(3) {
        vec![ALL_TERM, SUBSET_TERM]
    } else {
        vec![ALL_TERM]
    }
}

/// Parameterised over item count so the D-G concurrency tests near the end of this file (which
/// need `RowProjection::new` to take long enough to give a race a real window) can ask for a
/// larger synthetic corpus without duplicating the whole writer. [`build_fixture`] is the
/// `N_ITEMS`-sized default every other test in this file uses.
pub fn write_points_n(path: &Path, n: u64) {
    write_points_of(path, 0..n);
}

/// [`write_points_n`]'s points for the `entity_id`s `ids` alone.
pub fn write_points_of(path: &Path, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// See [`write_points_n`]'s doc.
pub fn write_pairs_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..n {
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

/// Build the fixture bundle at `out` through `mosaica_build::build`, over an
/// `n`-item synthetic corpus. See [`write_points_n`]'s doc for why this is parameterised.
pub fn build_fixture_n(out: &Path, points_path: &Path, pairs_path: &Path, n: u64) {
    build_fixture_sized(out, points_path, pairs_path, n, None)
}

/// [`build_fixture_n`], declaring `[shards] size` where `shard_size` is given.
pub fn build_fixture_sized(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    n: u64,
    shard_size: Option<u64>,
) {
    write_points_n(points_path, n);
    write_pairs_n(pairs_path, n);
    let schema = id_schema();
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        shard_size,
        strict: false,
        identity_key: test_key(),
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    build(&args).expect("fixture build should succeed");
}

/// Build the fixture bundle at `out` through `mosaica_build::build`.
pub fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    build_fixture_n(out, points_path, pairs_path, N_ITEMS)
}

/// A built fixture bundle and the paths an engine opens it with, holding the temporary directory
/// that owns all three.
pub struct Fixture {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub cache: PathBuf,
    pub wal: PathBuf,
}

impl Fixture {
    pub fn open(&self) -> Engine {
        open_engine_publishing(&self.root, &self.cache, &self.wal)
    }

    /// [`Fixture::open`] with the background refresh off, so a session stays on the generation it
    /// was handed until it asks again.
    pub fn open_unrefreshed(&self) -> Engine {
        let engine = self.open();
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The entities the build gave the generator's `sources`.
    pub fn members(&self, sources: impl IntoIterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        sources.into_iter().map(|s| EntityId::new(map[&s])).collect()
    }
}

/// The `N_ITEMS` fixture, built into a temporary directory of its own.
pub fn fixture() -> Fixture {
    fixture_sized(N_ITEMS, None)
}

/// [`fixture`] over `n` items, declaring `[shards] size` where `shard_size` is given.
pub fn fixture_sized(n: u64, shard_size: Option<u64>) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_sized(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        n,
        shard_size,
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

/// The same bundle, built into a caller's directory — for a case that opens the bundle more than
/// once, or beside paths of its own.
pub fn fixture_in(tmp: &Path) -> PathBuf {
    let root = tmp.join("bundle");
    build_fixture(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    root
}

/// Build a bundle over inputs the **corpus generator** wrote, with the generator's own schema.
///
/// Separate from [`build_fixture_n`] rather than a parameter on it: that one owns its inputs and
/// its (empty) schema, and the census owns neither — its points carry the generator's five column
/// families and its extent is the generator's. What the two share is `BuildArgs`, which is the
/// point.
pub fn build_corpus_fixture(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    corpus: &mosaica_corpus::Corpus,
) {
    // **Beside the sources, not beside the bundle.** A `source` is a path relative to the
    // document that declares it (`configuration.md` §3), and the generator's declaration names
    // `points.parquet` and `pairs.parquet` — so the document has to sit where they do.
    let config_path = points_path.with_file_name("corpus-config.toml");
    std::fs::create_dir_all(config_path.parent().expect("the sources have a parent")).ok();
    std::fs::write(&config_path, corpus.config_toml()).expect("the generator's config is writable");
    let config = mosaica_build::config::Config::parse(&config_path, &Default::default())
        .expect("the generator's config parses");
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: corpus.extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &config.schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        shard_size: None,
        strict: false,
        identity_key: test_key(),
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    build(&args).expect("the census fixture builds");
}

/// The generator's corpus built **with its layers** — the five artifact arms its declaration
/// carries, materialised beside the points and published into the bundle.
///
/// Separate from [`build_corpus_fixture`] rather than a flag on it: that one exists so a census can
/// register and publish its *own* layer against the generator's memberships, and reads none of the
/// artifact fixtures. This one is for the cases that are about the declaration — a layer built by
/// rule beside the same layer built by list — where what is under test is precisely what
/// `mosaica build` does with the generator's own `[[layer]]` blocks.
pub fn build_corpus_fixture_with_layers(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    corpus: &mosaica_corpus::Corpus,
) {
    let dir = points_path.parent().expect("the sources have a parent");
    corpus
        .write_artifact_fixtures(dir)
        .expect("the generator's artifact fixtures are writable");
    let config_path = points_path.with_file_name("corpus-config.toml");
    std::fs::write(&config_path, corpus.config_toml()).expect("the generator's config is writable");
    let config = mosaica_build::config::Config::parse(&config_path, &Default::default())
        .expect("the generator's config parses");
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: corpus.extent(),
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &config.schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        shard_size: None,
        strict: false,
        identity_key: test_key(),
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    build(&args).expect("the generator's own layers build");
}

/// The generator's points and access relation, under a **caller-supplied** declaration.
///
/// For the cases whose subject is a layer the generator does not declare — a spatial layer with
/// boxes, say, which its boundary arm has a roster for and no geometry. The points, the pairs and
/// the extent are the generator's, so its closed forms are still the oracle; the layers are the
/// case's own.
pub fn build_with_layers(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    corpus: &mosaica_corpus::Corpus,
    config: mosaica_build::config::Config,
) {
    build_with_layers_sized(out, points_path, pairs_path, corpus.extent(), config, None);
}

/// [`build_with_layers`] over `extent`, built at `shard_size`.
pub fn build_with_layers_sized(
    out: &Path,
    points_path: &Path,
    pairs_path: &Path,
    extent: Bounds,
    config: mosaica_build::config::Config,
    shard_size: Option<u64>,
) {
    let schema = with_id(config.schema);
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent,
            points: points_path.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points_path.to_path_buf(),
            &schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        shard_size,
        strict: false,
        identity_key: test_key(),
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    build(&args).expect("the fixture's own declaration builds");
}

/// The fixture's one declared column: `id`, the points file's `entity_id` declared unique, which
/// is how a test finds, or names in a row, the item a source row became.
pub fn id_schema() -> mosaica_build::config::Schema {
    mosaica_build::config::Schema {
        attributes: vec![mosaica_build::config::Attribute {
            field: Some("entity_id".to_string()),
            name: "id".to_string(),
            title: None,
            ty: mosaica_spatial::tiler::ScalarType::U64,
            analyser: None,
            vocabulary: None,
            value_set: None,
            index: false,
            render: false,
            unique: true,
        }],
        vocabularies: Default::default(),
    }
}

/// `schema` with [`id_schema`]'s `id` declared after its own columns, unless it declares one.
pub fn with_id(mut schema: mosaica_build::config::Schema) -> mosaica_build::config::Schema {
    if !schema.attributes.iter().any(|a| a.name == "id") {
        schema.attributes.extend(id_schema().attributes);
    }
    schema
}

/// Every `source_id -> entity_id` pair the bundle's unique `id` column holds in its runs.
pub fn source_to_new_map(bundle_root: &Path, prefix: &str) -> BTreeMap<u64, u64> {
    let bundle = open_bundle(bundle_root).unwrap();
    let runs = bundle
        .only_shard()
        .record
        .unique_indexes
        .iter()
        .find(|runs| runs.attribute == "id")
        .expect("the fixture declares a unique `id`");
    let mut map = BTreeMap::new();
    let paths = runs
        .base
        .iter()
        .map(|run| run.path.as_str())
        .chain(runs.live.iter().map(String::as_str));
    for rel in paths {
        mosaica_store::unique::for_each_entry(
            mosaica_store::unique::KeyKind::Unsigned,
            &bundle_root.join(prefix).join(rel),
            |key, entity| {
                if let mosaica_store::unique::UniqueKey::Int(source) = key {
                    map.insert(source, u64::from(entity));
                }
                Ok(())
            },
        )
        .unwrap();
    }
    map
}

/// Put `number` at `tenancy` in the built bundle's tenancy index, named and digested in its
/// side-manifest as a publication names it.
pub fn set_tenancy(bundle_root: &Path, number: EntityId, tenancy: u16) {
    use mosaica_store::manifest::TenancyBit;
    use mosaica_store::tenancy::bits_of;

    let prefix_dir = bundle_root.join("v00000");
    let bits = bits_of(&[(number.raw() as u32, tenancy)]);
    let mut manifest = mosaica_store::read_publication(&prefix_dir, PARTITION, 0)
        .unwrap()
        .manifest;
    let tenancy_rel = SHARD.tenancy_rel();
    for (bit, file) in mosaica_store::tenancy::write(&prefix_dir.join(&tenancy_rel), &bits).unwrap()
    {
        let name = file.file_name().unwrap().to_str().unwrap();
        let rel = format!("{tenancy_rel}/{name}");
        manifest
            .partition
            .files
            .insert(rel.clone(), mosaica_store::digest_of(&file).unwrap());
        manifest
            .only_shard_mut()
            .tenancy_index
            .push(TenancyBit { bit, path: rel });
    }
    rewrite_publication(&prefix_dir, 0, &manifest);
}

/// Give `shard` of the bundle `root` serves the high water `high_water` and the freed ids `free`,
/// each number at its tenancy in the shard's tenancy index, as a compaction that freed them leaves
/// its record.
pub fn seed_freed(root: &Path, shard: ShardId, high_water: u64, free: &[(u16, &[u32])]) {
    use mosaica_store::manifest::{EntitySet, TenancyBit};
    let prefix_dir = root.join(current_prefix(root));
    edit_side_manifest(root, |manifest| {
        let tenancies: Vec<(u32, u16)> = free
            .iter()
            .filter(|(tenancy, _)| *tenancy > 0)
            .flat_map(|(tenancy, ids)| ids.iter().map(move |id| (*id, *tenancy)))
            .collect();
        let tenancy_rel = mosaica_store::ShardLayout::new(PARTITION, shard).tenancy_rel();
        let bits = mosaica_store::tenancy::bits_of(&tenancies);
        let mut index = Vec::new();
        for (bit, file) in
            mosaica_store::tenancy::write(&prefix_dir.join(&tenancy_rel), &bits).unwrap()
        {
            let name = file.file_name().unwrap().to_str().unwrap();
            let rel = format!("{tenancy_rel}/{name}");
            manifest
                .partition
                .files
                .insert(rel.clone(), mosaica_store::digest_of(&file).unwrap());
            index.push(TenancyBit { bit, path: rel });
        }
        let record = manifest
            .shard_mut(shard)
            .expect("the bundle holds the shard");
        record.high_water = high_water;
        record.free_entities = EntitySet::of(&croaring::Bitmap::of(
            &free
                .iter()
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect::<Vec<u32>>(),
        ));
        record.tenancy_index = index;
    });
}

/// The item the unique `id` value `source` names, deleted items left out.
pub fn item_of_id(
    engine: &Engine,
    source: u64,
) -> Result<Option<EntityId>, mosaica_engine::EngineError> {
    Ok(unique_holders(engine, "id", &[source.to_string()])?[0].map(|item| item.entity))
}

/// The item each of the `id` values `ids` names, in its shard, `None` for one naming none.
pub fn items_by_id(engine: &Engine, ids: impl IntoIterator<Item = u64>) -> Vec<Option<ItemAt>> {
    let values: Vec<String> = ids.into_iter().map(|id| id.to_string()).collect();
    unique_holders(engine, "id", &values).expect("the `id` index reads")
}

/// The item the `id` value `id` names, in its shard.
pub fn item_by_id(engine: &Engine, id: u64) -> ItemAt {
    items_by_id(engine, [id])[0].expect("the value names an item")
}

/// The `id` a test gives the item it calls `key`: a stable hash with the top bit set, so it names
/// none of the fixture's built items.
pub fn key_id(key: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash | 1 << 63
}

/// The row values that give an item the `id` of `key`.
pub fn keyed(key: &str) -> Vec<mosaica_lifecycle::wal::WalScalar> {
    vec![mosaica_lifecycle::wal::WalScalar::U64(key_id(key))]
}

/// A new item at the origin holding the `id` of `key`.
pub fn keyed_row(key: &str) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: keyed(key),
        terms: Vec::new(),
        scoped: Vec::new(),
    }
}

/// The live item holding the `id` of `key`, if any.
pub fn item_of_key(engine: &Engine, key: &str) -> Option<EntityId> {
    unique_holders(engine, "id", &[key_id(key).to_string()]).unwrap()[0].map(|item| item.entity)
}

/// An engine with its write executor running.
///
/// **Publication is a submission now** (lifecycle §1.3): `Engine::publish_geometry` hands the
/// swap to the executor thread, so an engine that never started one answers `NoExecutor` rather
/// than publishing. That is the correct posture — there is no honest "published" when there is no
/// publisher — so a test that publishes starts the thread, exactly as one that submits a
/// `/control/changes` entry already had to.
pub fn open_engine_publishing(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    let mut engine = open_engine(bundle_root, cache_dir, wal_path);
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
    engine
}

/// The same, with the tick period cut to `secs`.
///
/// For a test whose subject is a cadence rather than a publication: the WAL gauge is sampled at
/// most once per `flush_max_age_secs` (`Executor::sample_wal_gauge`), so a test that reads it twice
/// either waits out the shipped 90 s or runs against a shorter period. Everything else keeps
/// [`config`]'s values.
pub fn open_engine_publishing_with_tick_period(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    secs: u64,
) -> Engine {
    let mut engine = Engine::open(
        bundle_root,
        cache_dir,
        wal_path,
        EngineConfig {
            flush_max_age_secs: secs,
            ..config()
        },
    )
    .expect("engine should open against a freshly built bundle");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
    engine
}

pub fn open_engine(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    Engine::open(bundle_root, cache_dir, wal_path, config())
        .expect("engine should open against a freshly built bundle")
}

pub fn open_engine_uncapped(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> Engine {
    Engine::open(bundle_root, cache_dir, wal_path, config_uncapped())
        .expect("engine should open against a freshly built bundle")
}

/// The fixture's whole extent, as a viewport request carries it.
pub const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// The artifacts a principal is served over the whole map at depth 0.
pub fn artifacts_of(engine: &Engine, credential: &[u8]) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport_artifacts(
            &session,
            mosaica_engine::ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX),
        )
        .expect("a viewport over the whole map")
        .artifacts()
}

/// Each artifact a tile-by-tile answer served, once, with its filter and highlight bits taken over
/// every tile it was served in: what one answer over the whole of the request's tiles says.
pub fn over_every_tile(out: &mosaica_engine::ViewportArtifactsOut) -> Vec<ArtifactOut> {
    let mut merged: Vec<ArtifactOut> = Vec::new();
    for artifact in out.frames.iter().flat_map(|frame| &frame.artifacts) {
        match merged
            .iter_mut()
            .find(|held| held.mosaica_id == artifact.mosaica_id)
        {
            Some(held) => {
                held.matched = held.matched.zip(artifact.matched).map(|(a, b)| a || b);
                held.highlighted = held
                    .highlighted
                    .zip(artifact.highlighted)
                    .map(|(a, b)| a || b);
            }
            None => merged.push(artifact.clone()),
        }
    }
    merged
}

/// The drawn shape a principal is served for an artifact, read by its identifier: the one route a
/// shape is served on.
pub fn shape_of(
    engine: &Engine,
    credential: &[u8],
    id: MosaicaId,
) -> Option<Vec<Vec<Vec<[u32; 2]>>>> {
    let session = engine.authorise(credential).unwrap();
    engine
        .artifact(&session, id, "s0", None)
        .unwrap()
        .expect("the artifact is served")
        .derived
        .shape
}

/// The change target of the item `entity` of the one shard.
pub fn item(entity: EntityId) -> mosaica_types::Target {
    mosaica_types::Target::Item(mosaica_types::ShardId::ZERO, entity)
}

/// What an artifact's or a layer's served identifier names.
pub fn artifact_target(engine: &Engine, id: MosaicaId) -> mosaica_types::Target {
    engine.resolve_targets(&[id]).unwrap()[0].expect("it names what was issued")
}

/// A one-row ingest at the fixture's centre, carrying `ALL_TERM`, under the caller's batch id.
pub fn ingest(engine: &Engine, batch: &str) -> EntityId {
    let row = UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    engine
        .ingest_rows(vec![row], batch.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

/// An engine over `root`, with its cache and log beside it under `tmp`, ticking every
/// `tick_secs` and its write executor running.
pub fn engine_at(tmp: &Path, root: &Path, tick_secs: u64) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

/// Whether the subset principal can see the item behind a source id.
pub fn subset_sees(e: u64) -> bool {
    terms_of(e).contains(&SUBSET_TERM)
}

/// A credential over the terms a generator grant names.
pub fn grant_credential(grant: &str) -> Vec<u8> {
    let terms: Vec<String> = mosaica_corpus::Grant::parse(grant)
        .expect("the grant is inside the generator's term space")
        .terms()
        .iter()
        .map(|t| format!("\"{}\"", t.raw()))
        .collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

pub fn full_coverage_credential() -> Vec<u8> {
    br#"{"terms": ["0"]}"#.to_vec()
}

pub fn subset_credential() -> Vec<u8> {
    br#"{"terms": ["1"]}"#.to_vec()
}

pub fn zero_credential() -> Vec<u8> {
    br#"{"terms": []}"#.to_vec()
}

/// The item each of `values` names in the unique field `field`, in its shard, `None` for a value
/// naming none, asked of [`mosaica_engine::Engine::name_items`].
pub fn unique_holders(
    engine: &mosaica_engine::Engine,
    field: &str,
    values: &[String],
) -> Result<Vec<Option<ItemAt>>, mosaica_engine::EngineError> {
    let table = mosaica_engine::AddressTable {
        rows: values.len(),
        mosaica_id: None,
        columns: vec![(
            field.to_string(),
            values
                .iter()
                .map(|v| Some(mosaica_engine::AddressValue::Text(v.clone())))
                .collect(),
        )],
    };
    Ok(engine
        .name_items(&table)?
        .verdicts
        .into_iter()
        .map(Result::ok)
        .collect())
}

/// Every WAL member file in `dir`, by name and bytes.
///
/// The log is a **sequence** — `wal-000001.log` and its `.sync` sidecar, beside the stem the engine
/// was opened with — not one file, so copying the stem copies nothing. Rotation seals a member,
/// opens the next and reclaims the ones behind it, which is exactly the state
/// a test of a restart before the rotation takes a copy of before the fold rotates.
pub fn snapshot_wal(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut members: Vec<(std::ffi::OsString, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("wal-"))
        .map(|e| (e.file_name(), std::fs::read(e.path()).unwrap()))
        .collect();
    assert!(
        !members.is_empty(),
        "the engine writes a WAL sequence, not one file"
    );
    members.sort_by(|a, b| a.0.cmp(&b.0));
    members
}

/// Put `snapshot` back, removing whatever members are there now — the on-disc state a crash between
/// the `CURRENT` flip and the WAL rotation leaves.
pub fn restore_wal(dir: &Path, snapshot: &[(std::ffi::OsString, Vec<u8>)]) {
    for entry in std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with("wal-") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    for (name, bytes) in snapshot {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// A flat, enumerated layer over `s0` that an artifact is served in when two of its members are
/// visible, computing no content.
pub fn flat_declaration(name: &str) -> mosaica_types::layer::LayerDeclaration {
    use mosaica_types::layer::*;
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: ArtifactVisibility::inherited(),
        require_member_visibility: Some(ExistenceCriterion::Count(2)),
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: Default::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// A flat layer that derives a hull, so its levels hold their rows: the declaration a test building
/// forms directly through `ArtifactProjections::get_or_build` builds them for.
pub fn held_rows_declaration(name: &str) -> mosaica_types::layer::LayerDeclaration {
    use mosaica_types::layer::*;
    LayerDeclaration {
        title: None,
        require_member_visibility: None,
        content: ContentDeclaration {
            computed: vec!["hull".into()],
            supplied: Vec::new(),
        },
        ..flat_declaration(name)
    }
}

/// Wait for the side-manifest to list at least `want` artifact content extents.
pub fn wait_for_content_extents(root: &Path, want: usize) {
    let published = || {
        let manifest = side_manifest(root);
        manifest.layer_spaces.iter().map(|r| r.content_extents.len()).sum::<usize>()
    };
    wait_until(
        &format!("{want} content extents published"),
        std::time::Duration::from_secs(10),
        || published() >= want,
    );
}

pub fn limits() -> RecordsLimits {
    RecordsLimits {
        max_page_rows: 100_000,
        max_page_bytes: 64 << 20,
        response_bytes: 256 << 20,
        response_time: Duration::from_secs(60),
    }
}

pub fn caps() -> AggregateCaps {
    AggregateCaps {
        groupings: 8,
        top: 100,
        named: 100,
        bins: 100,
        cells: u64::MAX,
    }
}

/// What one records or aggregate response served: a records response's head, and every page.
#[derive(Default)]
pub struct Pages {
    pub head: Option<RecordsHead>,
    pub batches: Vec<RecordBatch>,
}

impl RecordsSink for Pages {
    fn head(&mut self, head: &RecordsHead) -> SinkResult {
        self.head = Some(head.clone());
        Ok(())
    }

    fn page(&mut self, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.batches.push(batch.clone());
        Ok(())
    }
}

impl AggregateSink for Pages {
    fn head(&mut self, _: &AggregateHead) -> SinkResult {
        Ok(())
    }

    fn table(&mut self, _: &TableHead) -> SinkResult {
        Ok(())
    }

    fn page(&mut self, _: u32, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.batches.push(batch.clone());
        Ok(())
    }
}

/// A read resumed from each response's cursor to the end: `read(cursor, pages)` serves one
/// response into `pages` and answers its cursor. Every response, in order.
pub fn drain(mut read: impl FnMut(Option<&str>, &mut Pages) -> Option<String>) -> Vec<Pages> {
    let mut responses = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut pages = Pages::default();
        cursor = read(cursor.as_deref(), &mut pages);
        responses.push(pages);
        if cursor.is_none() {
            return responses;
        }
    }
}

/// Each row's values as `name=value`, but those of the columns `skip` names. `key`, where given,
/// names a `UInt64` `key` column's identifier in place of its value.
pub fn rows_of(
    batch: &RecordBatch,
    skip: &[&str],
    key: Option<&dyn Fn(MosaicaId) -> String>,
) -> Vec<Vec<String>> {
    let schema = batch.schema();
    (0..batch.num_rows())
        .map(|row| {
            schema
                .fields()
                .iter()
                .zip(batch.columns())
                .filter(|(field, _)| !skip.contains(&field.name().as_str()))
                .map(|(field, column)| match (field.name().as_str(), key) {
                    ("key", Some(key))
                        if column.is_valid(row) && column.data_type() == &DataType::UInt64 =>
                    {
                        let id = column.as_any().downcast_ref::<UInt64Array>().unwrap();
                        key(MosaicaId::new(id.value(row)))
                    }
                    _ => format!(
                        "{}={}",
                        field.name(),
                        arrow::util::display::array_value_to_string(column, row).unwrap()
                    ),
                })
                .collect()
        })
        .collect()
}

/// An items read of view `s0`, resumed from each response's cursor to the end.
#[derive(Clone)]
pub struct Items {
    pub order: RecordsOrder,
    /// Declared fields; `id` first.
    pub fields: Vec<String>,
    pub filter: Option<FilterExpr>,
    pub keep_unmatched: bool,
    pub page_rows: u32,
}

impl Items {
    /// `id` alone, in `order`.
    pub fn new(order: RecordsOrder) -> Items {
        Items {
            order,
            fields: vec!["id".to_string()],
            filter: None,
            keep_unmatched: false,
            page_rows: 7,
        }
    }

    pub fn filter(mut self, filter: FilterExpr) -> Items {
        self.filter = Some(filter);
        self
    }
}

/// What an items read served.
pub struct ItemsRead {
    /// Each page's rows, in order, without `mosaica_id`.
    pub pages: Vec<Vec<Vec<String>>>,
    /// The first response's counts.
    pub counts: Option<RecordsCounts>,
    /// Every row's `(mosaica_id, id)`.
    pub rows_with_ids: Vec<(u64, u64)>,
}

pub fn read_items(engine: &Engine, session: &Session, read: &Items) -> ItemsRead {
    let system = ["position".to_string(), "labels".to_string()];
    let mut responses = drain(|cursor, pages| {
        engine
            .items_stream(
                session,
                ItemsRequest {
                    view: "s0",
                    fields: &read.fields,
                    system_fields: &system,
                    filter: read.filter.clone(),
                    keep_unmatched: read.keep_unmatched,
                    count: cursor.is_none(),
                    order: Some(read.order),
                    page_rows: Some(read.page_rows),
                    pages: Some(1),
                    cursor,
                    limits: limits(),
                    cancel: None,
                },
                pages,
            )
            .expect("the items read answers")
            .next
    });
    let mut out = ItemsRead {
        pages: Vec::new(),
        counts: responses[0].head.take().and_then(|head| head.counts),
        rows_with_ids: Vec::new(),
    };
    for batch in responses.iter().flat_map(|response| &response.batches) {
        out.pages.push(rows_of(batch, &["mosaica_id"], None));
        let column = |name: &str| -> Vec<u64> {
            let values = batch.column_by_name(name).unwrap();
            values.as_any().downcast_ref::<UInt64Array>().unwrap().values().to_vec()
        };
        out.rows_with_ids
            .extend(column("mosaica_id").into_iter().zip(column("id")));
    }
    out
}
