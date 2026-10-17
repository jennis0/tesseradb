//! The two-shard fixture and what the several-shard cases read of it: a bundle built past its
//! `[shards] size`, an engine opened to write several shards, and what a viewer is served, to
//! compare with the same items built as one shard.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use arrow::array::UInt64Array;
use arrow::record_batch::RecordBatch;
use mosaica_engine::{
    AggregateRequest, Engine, EngineConfig, Grouping, RecordsOrder, ScalarOut, Session,
    ViewportRequest,
};
use mosaica_lifecycle::faults::FaultSwitchboard;
use mosaica_lifecycle::BatchArtifacts;
use mosaica_store::manifest::{EntitySet, SegmentsManifest};
use mosaica_types::{EntityId, MosaicaId, ShardId, ShardState, Target};

use super::*;

/// Items the two-shard fixture builds.
pub const TWO_SHARD_N: u64 = 40;
/// The size the two-shard fixture is built at: shard 0 sealed at 25 items, shard 1 open at 15.
pub const TWO_SHARD_SIZE: u64 = 25;

/// The fixture over [`TWO_SHARD_N`] items, built as two shards.
pub fn two_shards() -> Fixture {
    fixture_sized(TWO_SHARD_N, Some(TWO_SHARD_SIZE))
}

pub fn shard(id: u32) -> ShardId {
    ShardId::new(id).unwrap()
}

/// Shard `shard`'s item `entity`, as a change names it.
pub fn item_in(shard_id: u32, entity: u64) -> Target {
    Target::Item(shard(shard_id), EntityId::new(entity))
}

/// The two-shard fixture's `target` as the same items built as one shard name it: shard 1's
/// entities follow shard 0's.
pub fn as_one_shard(target: Target) -> Target {
    match target {
        Target::Item(at, entity) if at == shard(1) => item_in(0, TWO_SHARD_SIZE + entity.raw()),
        other => other,
    }
}

/// The entities a side-manifest's set names.
pub fn entity_ids(set: &EntitySet) -> Vec<u32> {
    set.entities().expect("the set decodes").iter().collect()
}

fn open_several(root: &Path, cache: &Path, wal: &Path, config: EngineConfig) -> Engine {
    Engine::open_writing_several_shards(root, cache, wal, config).expect("the engine opens")
}

/// An engine over `root` that writes several shards, its executor running.
pub fn open_writing(root: &Path, cache: &Path, wal: &Path, config: EngineConfig) -> Engine {
    let mut engine = open_several(root, cache, wal, config);
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

/// [`open_writing`], its executor running under the faults the switchboard arms.
pub fn open_writing_faulted(
    root: &Path,
    cache: &Path,
    wal: &Path,
    config: EngineConfig,
) -> (Engine, Arc<FaultSwitchboard>) {
    let mut engine = open_several(root, cache, wal, config);
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(8, Arc::clone(&faults))
        .expect("the executor starts");
    (engine, faults)
}

impl Fixture {
    /// [`open_writing`] over this fixture under [`config`].
    pub fn open_writing(&self) -> Engine {
        open_writing(&self.root, &self.cache, &self.wal, config())
    }
}

/// The `id`s below `n` whose items are in `shard`.
pub fn ids_in_shard(engine: &Engine, n: u64, shard_id: u32) -> Vec<u64> {
    (0..n)
        .zip(items_by_id(engine, 0..n))
        .filter(|(_, item)| item.is_some_and(|item| item.shard == shard(shard_id)))
        .map(|(id, _)| id)
        .collect()
}

/// One batch of new items, one for each of `labelled`: row `at` holds the `id` of the key
/// `{batch}-{at}`, sits at (5, 5), and carries the term `0` where it is labelled and no term where
/// it is not. The receipt's `mosaica_id`s, in row order.
pub fn insert_keyed(engine: &Engine, batch: &str, labelled: &[bool]) -> Vec<MosaicaId> {
    insert_keyed_joining(engine, batch, labelled, BatchArtifacts::default())
}

/// [`insert_keyed`], with the batch's memberships and edges.
pub fn insert_keyed_joining(
    engine: &Engine,
    batch: &str,
    labelled: &[bool],
    artifacts: BatchArtifacts,
) -> Vec<MosaicaId> {
    let rows = labelled
        .iter()
        .enumerate()
        .map(|(at, labelled)| mosaica_lifecycle::IngestRow {
            mosaica_id: None,
            labels: Some(match labelled {
                true => vec![b"0".to_vec()],
                false => Vec::new(),
            }),
            position: Some((5.0, 5.0)),
            scalars: keyed(&format!("{batch}-{at}")),
            scoped: Vec::new(),
            omitted: Vec::new(),
        })
        .collect();
    let receipt = engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: batch.to_string(),
            body_hash: [9; 32],
            view: Some("s0".to_string()),
            rows,
            artifacts,
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the batch is accepted");
    receipt
        .mosaica_ids
        .into_iter()
        .map(|id| id.expect("each row created an item"))
        .collect()
}

/// One batch moving the item of the `id` value `id` to (7, 7), labelled `0`: an edit. The
/// `mosaica_id` its receipt answers.
pub fn edit_by_id(
    engine: &Engine,
    batch: &str,
    id: u64,
) -> Result<MosaicaId, mosaica_engine::AcceptError> {
    let receipt = engine.ingest(mosaica_engine::IngestRequest {
        batch_id: batch.to_string(),
        body_hash: [4; 32],
        view: Some("s0".to_string()),
        rows: vec![mosaica_lifecycle::IngestRow {
            mosaica_id: None,
            labels: Some(vec![b"0".to_vec()]),
            position: Some((7.0, 7.0)),
            scalars: vec![mosaica_lifecycle::wal::WalScalar::U64(id)],
            scoped: Vec::new(),
            omitted: Vec::new(),
        }],
        artifacts: Default::default(),
        strict: true,
        mosaica_id_column: false,
    })?;
    Ok(receipt.mosaica_ids[0].expect("the row names the item"))
}

/// Remove every partition file published after publication `n`, as a crash before those
/// publications committed leaves the bundle.
pub fn crash_back_to(root: &Path, n: u64) {
    let dir = partition_dir(root, &current_prefix(root));
    for later in n + 1..=served_n(root) {
        let path = dir.join(mosaica_store::layout::segments_name(later));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
}

/// Each shard's id, state, high water, freed ids and number of held ids as a side-manifest
/// records them, and its `next_id`.
pub type Recorded = (Vec<(u32, ShardState, u64, Vec<u32>, usize)>, u32);

pub fn recorded(manifest: &SegmentsManifest) -> Recorded {
    let shards = manifest
        .shards
        .records
        .iter()
        .map(|record| {
            (
                record.id.raw(),
                record.state,
                record.high_water,
                entity_ids(&record.free_entities),
                record.held_entities.len(),
            )
        })
        .collect();
    (shards, manifest.shards.next_id)
}

fn u64s(batch: &RecordBatch, name: &str) -> Vec<u64> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("the page carries `{name}`"))
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap()
        .values()
        .to_vec()
}

/// Each tile's visible count at `depth` over the whole map.
pub fn tile_counts(engine: &Engine, session: &Session, depth: u8) -> Vec<(u64, u64)> {
    let out = engine
        .viewport(
            session,
            ViewportRequest::new("s0", depth, WHOLE_MAP, TWO_SHARD_N as usize),
        )
        .expect("the viewport answers");
    let mut counts: Vec<(u64, u64)> = out.tiles.iter().map(|t| (t.tile, t.visible)).collect();
    counts.sort_unstable();
    counts
}

/// Each cell's count at `depth`, from the aggregate.
pub fn cell_counts(engine: &Engine, session: &Session, depth: u8) -> Vec<(u64, u64)> {
    let groupings = [Grouping {
        by: None,
        cells: Some(depth),
        area: None,
    }];
    let mut pages = Pages::default();
    engine
        .aggregate_stream(
            session,
            AggregateRequest {
                view: "s0",
                filter: None,
                reference: None,
                groupings: &groupings,
                page_rows: None,
                pages: None,
                cursor: None,
                limits: limits(),
                caps: caps(),
                cancel: None,
            },
            &mut pages,
        )
        .expect("the aggregate answers");
    let mut counts: Vec<(u64, u64)> = pages
        .batches
        .iter()
        .flat_map(|batch| u64s(batch, "cell").into_iter().zip(u64s(batch, "count")))
        .collect();
    counts.sort_unstable();
    counts
}

/// The `id` the item card of `mosaica_id` carries, or `None` where the viewer is served no card.
pub fn card_id(engine: &Engine, session: &Session, mosaica_id: u64) -> Option<u64> {
    let card = engine
        .item(session, MosaicaId::new(mosaica_id))
        .expect("the item card answers")?;
    card.fields
        .iter()
        .find(|field| field.name == "id")
        .map(|field| match field.value {
            ScalarOut::U64(id) => id,
            ref other => panic!("`id` is a u64, not {other:?}"),
        })
}

/// What a viewer is served, by the fixture's own `id`, which a bundle of one shard and one of
/// several built over the same items share.
#[derive(Debug, PartialEq)]
pub struct Served {
    /// The items a bulk read answers, each checked against its card.
    pub ids: Vec<u64>,
    /// Each tile's visible count at depth 3.
    pub tiles: Vec<(u64, u64)>,
    /// Each cell's count at depth 4, from the aggregate.
    pub cells: Vec<(u64, u64)>,
}

pub fn served_to(engine: &Engine, credential: &[u8]) -> Served {
    let session = engine.authorise(credential).unwrap();
    let read = read_items(engine, &session, &Items::new(RecordsOrder::Stored)).rows_with_ids;
    for &(mosaica_id, id) in &read {
        assert_eq!(
            card_id(engine, &session, mosaica_id),
            Some(id),
            "item {id}'s card"
        );
    }
    let mut ids: Vec<u64> = read.iter().map(|&(_, id)| id).collect();
    ids.sort_unstable();
    Served {
        ids,
        tiles: tile_counts(engine, &session, 3),
        cells: cell_counts(engine, &session, 4),
    }
}

/// `engine` serves the full and the subset credential what `one_engine` serves them.
pub fn assert_serves_as(engine: &Engine, one_engine: &Engine, when: &str) {
    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(
            served_to(engine, &credential),
            served_to(one_engine, &credential),
            "{when}"
        );
    }
}
