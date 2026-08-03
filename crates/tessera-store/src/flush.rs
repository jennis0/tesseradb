//! What a flush writes into an existing bundle (§3.1).
//!
//! Per segment: `morton.u32`, `columns.arrow`, an `external-ids.arrow` extent and an
//! `ext-locator.u32` extent, plus the descriptor, the row-space extent and the digests the new
//! `SEGMENTS-<n+1>.json` names them by. **Never `MANIFEST.json`, never `CURRENT`** — a flush
//! publishes inside the current prefix, which is what separates it from a compaction.
//!
//! ## What is deliberately not here
//!
//! **The delta postings tier.** It is `tessera-authz`-shaped ([`tessera_authz::write_delta_tier`])
//! and this crate does not depend on that one — `read.rs`'s module doc states the property, and
//! the layering is what keeps `RowId` out of the authorisation crate. The flush unit in
//! `tessera-engine` writes both halves and folds both digests into the one manifest, exactly as
//! `tessera-build` already composes store and authz writers.
//!
//! **The dictionary extent**, for the same reason: a promoted descriptor's durable ordinal is
//! written through `tessera_authz::DictStreamWriter`.
//!
//! ## The watermark is `entity_hi + 1`
//!
//! Composition treats entities at or above the watermark as buffer-resident, so a watermark of
//! `entity_hi` would leave the highest flushed entity excluded from the fragment *and* absent from
//! the buffer — invisible, with a row, for ever. [`FlushOutput::watermark`] is the value the
//! caller must publish, and the test that pins it is named for that consequence rather than for
//! the arithmetic.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryArray, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;
use sha2::{Digest, Sha256};

use tessera_spatial::fixed32;
use tessera_spatial::tiler::{sort_batch, ScalarType, ScalarValue, TilerItem};
use tessera_types::{EntityId, IdentityKey, TesseraId, ROW_ABSENT};

use crate::error::{Result, StoreError};
use crate::manifest::{FileDigest, LocatorExtent, Quantisation, SegmentDescriptor};
use crate::permutation::SegmentExtent;
use crate::write::write_segment;

/// One item a flush is about to give geometry to.
///
/// Store-shaped rather than `tessera_lifecycle::BufferedItem`, because this crate does not depend
/// on that one; the engine converts. `x`/`y` are still the caller's coordinates — quantisation
/// happens here, once, against the bundle's own `quantisation` (contracts §2.5), so there is no
/// second place a coordinate could become a cell under bounds that have drifted.
#[derive(Debug, Clone, PartialEq)]
pub struct FlushRow {
    pub entity_id: EntityId,
    /// `None` for an item ingested without one (contracts §3.4 r6): addressable only by its
    /// `tessera_id`, present in no external-id extent, and given the locator's absent sentinel.
    pub external_id: Option<Vec<u8>>,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<ScalarValue>,
}

/// Everything one flush needs to write one segment.
pub struct FlushInput<'a> {
    pub seg_id: &'a str,
    /// **Ascending by `entity_id`, with deleted entities already removed** (§3.5: a deletion's ID
    /// stays burned and no row is created for it). Contiguity is I9's doing — ids are issued
    /// monotonically from the high-water — and it is what makes the extent dense.
    pub rows: Vec<FlushRow>,
    pub quantisation: Quantisation,
    pub identity_key: &'a IdentityKey,
    pub shard_id: u32,
    pub scalar_schema: &'a [(String, ScalarType)],
    /// Where this segment's rows begin in the slice's row space — the slice's current total.
    pub row_base: u32,
}

/// What a flush produced, for the caller to name in `SEGMENTS-<n+1>.json` and hand to
/// [`crate::Bundle::with_segment`].
#[derive(Debug)]
pub struct FlushOutput {
    pub segment: SegmentDescriptor,
    pub extent: SegmentExtent,
    /// Prefix-relative path of the forward (external_id → entity) extent.
    pub external_id_run: String,
    pub locator_extent: LocatorExtent,
    /// Every file written, prefix-relative, for the manifest's `files` map.
    pub files: BTreeMap<String, FileDigest>,
    /// `entity_hi + 1` — see this module's doc.
    pub watermark: u64,
    pub entity_id_high_water: u64,
}

/// Write one flush segment under `prefix_dir`, for `(partition, slice)`.
///
/// Returns without fsyncing the directory: the caller's commit point is the side-manifest, and it
/// is responsible for making every file here durable **before** writing it (§7.3).
pub fn write_flush_segment(
    prefix_dir: &Path,
    partition: &str,
    slice: &str,
    input: FlushInput<'_>,
) -> Result<FlushOutput> {
    if input.rows.is_empty() {
        return Err(StoreError::MalformedBundle {
            detail: "write_flush_segment: a segment with no rows is not publishable".to_string(),
        });
    }
    if !input
        .rows
        .windows(2)
        .all(|w| w[0].entity_id < w[1].entity_id)
    {
        return Err(StoreError::MalformedBundle {
            detail: "write_flush_segment: rows must be strictly ascending by entity id (I9's \
                     monotone allocation is what makes the extent dense)"
                .to_string(),
        });
    }

    let entity_lo = input.rows[0].entity_id.raw();
    let entity_hi = input.rows[input.rows.len() - 1].entity_id.raw();
    let span =
        usize::try_from(entity_hi - entity_lo + 1).map_err(|_| StoreError::MalformedBundle {
            detail: format!(
                "write_flush_segment: entity span {entity_lo}..={entity_hi} is too wide"
            ),
        })?;

    let rel = |name: &str| {
        format!(
            "partitions/{partition}/slices/{slice}/segments/{}/{name}",
            input.seg_id
        )
    };
    let seg_dir = prefix_dir
        .join("partitions")
        .join(partition)
        .join("slices")
        .join(slice)
        .join("segments")
        .join(input.seg_id);
    fs::create_dir_all(&seg_dir).map_err(|source| StoreError::Io {
        path: seg_dir.clone(),
        source,
    })?;

    // ---- geometry ---------------------------------------------------------------------------
    //
    // Quantised against the bundle's own extent and sorted by the resulting Morton code. The same
    // global quantisation every other segment used, never a per-segment one: `tile_ranges`'
    // binary search resolves a tile to one contiguous range **per segment**, and a segment coded
    // against different bounds would answer that search with rows from the wrong cells.
    let q = &input.quantisation;
    let mut items: Vec<TilerItem> = Vec::with_capacity(input.rows.len());
    for row in &input.rows {
        items.push(TilerItem {
            tessera_id: tessera_id_of(input.identity_key, input.shard_id, row.entity_id)?,
            qx: fixed32(row.x as f64, q.x_min, q.x_max),
            qy: fixed32(row.y as f64, q.y_min, q.y_max),
            scalars: row.scalars.clone(),
        });
    }
    let mut entity_ids: Vec<EntityId> = input.rows.iter().map(|r| r.entity_id).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);
    write_segment(&seg_dir, &items, &codes, input.scalar_schema).map_err(|source| {
        StoreError::Io {
            path: seg_dir.join("columns.arrow"),
            source,
        }
    })?;

    // The extent's `rows[e - entity_lo]` is the entity's position in the sorted order, relative to
    // `row_base`. Written here rather than as a `permutation.bin`, whose length is the *bundle's*
    // whole entity space — the wrong shape for a segment covering a few thousand ids at the top of
    // it. Every slot is filled: deleted entities were removed by the caller, so the span is dense.
    let mut extent_rows = vec![ROW_ABSENT; span];
    for (row, entity) in entity_ids.iter().enumerate() {
        extent_rows[(entity.raw() - entity_lo) as usize] = row as u32;
    }

    // ---- the external-id directions (§3.6) --------------------------------------------------
    //
    // Forward: external_id → entity, sorted by the id bytes, because the reader binary-searches
    // it. Reverse: entity → this extent's ordinal, dense over the entity span. Both, because the
    // reverse direction is served live-map-first and locator-second, and rotation empties the live
    // map at restart — without a durable reverse path a visible flushed item would answer
    // `/v1/items` with a typed error for ever.
    let mut forward: Vec<(&[u8], u32)> = input
        .rows
        .iter()
        .filter_map(|r| {
            let id = r.external_id.as_deref()?;
            u32::try_from(r.entity_id.raw()).ok().map(|e| (id, e))
        })
        .collect();
    forward.sort_unstable_by(|a, b| a.0.cmp(b.0));

    let external_id_rel = rel("external-ids.arrow");
    write_external_id_run(&seg_dir.join("external-ids.arrow"), &forward)?;

    let mut locator = vec![ROW_ABSENT; span];
    for (ordinal, (_, entity)) in forward.iter().enumerate() {
        locator[(*entity as u64 - entity_lo) as usize] = ordinal as u32;
    }
    let locator_rel = rel("ext-locator.u32");
    write_u32_array(&seg_dir.join("ext-locator.u32"), &locator)?;

    // ---- what the manifest must name --------------------------------------------------------
    let mut files = BTreeMap::new();
    for name in [
        "morton.u32",
        "columns.arrow",
        "external-ids.arrow",
        "ext-locator.u32",
    ] {
        files.insert(rel(name), digest_of(&seg_dir.join(name))?);
    }

    Ok(FlushOutput {
        segment: SegmentDescriptor {
            slice: slice.to_string(),
            seg_id: input.seg_id.to_string(),
            row_count: input.rows.len() as u32,
            entity_lo,
            entity_hi,
        },
        extent: SegmentExtent {
            entity_lo,
            entity_hi,
            seg_id: input.seg_id.to_string(),
            row_base: input.row_base,
            rows: extent_rows,
        },
        external_id_run: external_id_rel.clone(),
        locator_extent: LocatorExtent {
            path: locator_rel,
            entity_lo,
            entity_hi,
            external_id_run: external_id_rel,
        },
        files,
        // Composition treats entities at or above the watermark as buffer-resident, so this is
        // `entity_hi + 1` and not `entity_hi`. See the module doc.
        watermark: entity_hi + 1,
        entity_id_high_water: entity_hi + 1,
    })
}

fn tessera_id_of(key: &IdentityKey, shard_id: u32, entity: EntityId) -> Result<TesseraId> {
    key.forward(shard_id, entity)
        .map_err(|e| StoreError::MalformedBundle {
            detail: format!(
                "write_flush_segment: tessera_id for entity {}: {e}",
                entity.raw()
            ),
        })
}

/// One external-id extent: `external_id: Binary` and `entity_id: UInt32`, ascending by the id
/// bytes. The shape `crate::sidecar` binary-searches, and it verifies that sortedness at open —
/// so an unsorted extent is a refusal there rather than a wrong answer here.
pub(crate) fn write_external_id_run(path: &Path, rows: &[(&[u8], u32)]) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("entity_id", DataType::UInt32, false),
    ]));
    let external: ArrayRef = Arc::new(BinaryArray::from_iter_values(
        rows.iter().map(|(id, _)| *id),
    ));
    let entity: ArrayRef = Arc::new(UInt32Array::from_iter_values(rows.iter().map(|(_, e)| *e)));
    let batch = RecordBatch::try_new(schema.clone(), vec![external, entity]).map_err(|e| {
        StoreError::MalformedBundle {
            detail: format!("external-ids.arrow: {e}"),
        }
    })?;

    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = File::create(path).map_err(io)?;
    let mut writer = FileWriter::try_new(BufWriter::new(file), &schema).map_err(|e| {
        StoreError::MalformedBundle {
            detail: format!("external-ids.arrow: {e}"),
        }
    })?;
    writer
        .write(&batch)
        .map_err(|e| StoreError::MalformedBundle {
            detail: format!("external-ids.arrow: {e}"),
        })?;
    writer.finish().map_err(|e| StoreError::MalformedBundle {
        detail: format!("external-ids.arrow: {e}"),
    })?;
    Ok(())
}

/// A raw little-endian `u32` array, no header — the `ext-locator.u32` shape (contracts §2.4 r6),
/// here over one segment's entity range rather than the whole entity space.
pub(crate) fn write_u32_array(path: &Path, values: &[u32]) -> Result<()> {
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = File::create(path).map_err(io)?;
    let mut writer = BufWriter::with_capacity(1 << 16, file);
    for value in values {
        writer.write_all(&value.to_le_bytes()).map_err(io)?;
    }
    writer.flush().map_err(io)?;
    Ok(())
}

pub(crate) fn digest_of(path: &Path) -> Result<FileDigest> {
    let bytes = fs::read(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(FileDigest {
        size: bytes.len() as u64,
        sha256: hex,
    })
}
