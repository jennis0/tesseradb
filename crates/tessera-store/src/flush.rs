//! What a flush writes into an existing bundle.
//!
//! Per segment: `morton.u32`, `columns.arrow`, and, where a row binds an entity rather than
//! joining one, an `external-ids.arrow` run and an `ext-locator.u32` extent, plus the descriptor,
//! the row-space extent and the digests the new `SEGMENTS-<n+1>.json` names them by. A flush
//! never writes `MANIFEST.json` or `CURRENT`: it publishes inside the current prefix, which is
//! what separates it from a compaction.
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

use sha2::{Digest, Sha256};

use tessera_spatial::fixed32;
use tessera_spatial::tiler::{sort_batch, ScalarType, ScalarValue, TilerItem};
use tessera_types::{EntityId, IdentityKey, TesseraId, ROW_ABSENT};

use crate::error::{Result, StoreError};
use crate::manifest::{FileDigest, LocatorExtent, Quantisation, SegmentDescriptor};
use crate::permutation::SegmentExtent;
use crate::render_presence::{render_presence_path, RenderPresence, RENDER_PRESENCE_DIR};
use crate::write::{write_segment, RunWriter};

/// One item a flush is about to give geometry to.
///
/// Store-shaped rather than `tessera_lifecycle::BufferedItem`, because this crate does not depend
/// on that one; the engine converts. `x`/`y` are still the caller's coordinates — quantisation
/// happens here, once, against the bundle's own `quantisation` (contracts §2.5), so there is no
/// second place a coordinate could become a cell under bounds that have drifted.
///
/// They are `f64` because the whole coordinate path is (`projections.md` §6), and this is its last
/// stop: an `f32` here would decide the cell rather than the sub-cell position at any frame past
/// roughly zoom offset 8, and would do it after the wire and the log had both carried the value
/// the caller sent.
#[derive(Debug, Clone, PartialEq)]
pub struct FlushRow {
    pub entity_id: EntityId,
    /// `None` for an item ingested without one (contracts §3.4 r6): addressable only by its
    /// `tessera_id`, present in no external-id extent, and given the locator's absent sentinel.
    pub external_id: Option<Vec<u8>>,
    pub x: f64,
    pub y: f64,
    /// One value per **render** column, in declared order. [`ScalarValue::Null`] is a legal member
    /// and is the caller's only way to say "this item has no value here": the writer records it in
    /// the column's presence bitmap and stores the type's zero in the row (decision 0064).
    /// Substituting the zero *before* this point loses the distinction irrecoverably.
    pub scalars: Vec<ScalarValue>,
}

/// Everything one flush needs to write one segment.
pub struct FlushInput<'a> {
    pub seg_id: &'a str,
    /// The incarnation of the view this segment is written into (decision 0115), stamped into the
    /// descriptor so that a key created again cannot adopt it.
    pub incarnation: tessera_types::view::ViewIncarnation,
    /// **Ascending by `entity_id`, with deleted entities already removed** (§3.5: a deletion's ID
    /// stays burned and no row is created for it). Contiguity is I9's doing — ids are issued
    /// monotonically from the high-water — and it is what makes the extent dense.
    pub rows: Vec<FlushRow>,
    pub quantisation: Quantisation,
    pub identity_key: &'a IdentityKey,
    pub shard_id: u32,
    pub scalar_schema: &'a [(String, ScalarType)],
    /// Where this segment's rows begin in the view's row space — the view's current total.
    pub row_base: u32,
}

/// What a flush produced, for the caller to name in `SEGMENTS-<n+1>.json` and hand to
/// [`crate::Bundle::with_segment`].
#[derive(Debug)]
pub struct FlushOutput {
    pub segment: SegmentDescriptor,
    pub extent: SegmentExtent,
    /// The locator extent over the rows that bind, naming the run it indexes; `None` where every
    /// row is a join.
    pub locator_extent: Option<LocatorExtent>,
    /// Every file written, prefix-relative, for the manifest's `files` map.
    pub files: BTreeMap<String, FileDigest>,
    /// `entity_hi + 1` — see this module's doc.
    pub watermark: u64,
    pub entity_id_high_water: u64,
}

/// Write one flush segment under `prefix_dir`, for `(partition, view)`. `joins` names the
/// entities, ascending, whose rows join an entity an earlier row already bound: a join writes no
/// external-id entry and no locator slot, and the locator extent spans the binding rows alone.
///
/// Returns without fsyncing the directory: the caller's commit point is the side-manifest, and it
/// is responsible for making every file here durable before writing it.
pub fn write_flush_segment(
    prefix_dir: &Path,
    partition: &str,
    view: &str,
    input: FlushInput<'_>,
    joins: &[EntityId],
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

    // **`view_rel`, never the joined id** (`views.md` §3.2): a group's view lays its files down
    // at `views/<group>/<key>/` and a `files` key spelled `views/<group>:<key>/` names a path no
    // reader will look at — every file under the view unverifiable, which the loader reads as a
    // corrupt bundle. The path below and the key here must come from one derivation.
    let rel = |name: &str| {
        format!(
            "partitions/{partition}/{}/segments/{}/{name}",
            crate::view_rel(view),
            input.seg_id
        )
    };
    let seg_dir = crate::view_path(&prefix_dir.join("partitions").join(partition), view)
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
            qx: fixed32(row.x, q.x_min, q.x_max),
            qy: fixed32(row.y, q.y_min, q.y_max),
            scalars: row.scalars.clone(),
        });
    }
    let mut entity_ids: Vec<EntityId> = input.rows.iter().map(|r| r.entity_id).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    // ---- the render columns' presence (decision 0064) ---------------------------------------
    //
    // After the sort, because the bitmap is over **rows** and the sort is what decides them.
    //
    // A [`ScalarValue::Null`] in a render column is a *non-category* absence by construction: the
    // ingest plane resolves a category's missing key to the reserved code 0 its vocabulary keeps
    // out of the value space, before the row is ever buffered. So keying off `Null` writes a
    // bitmap for exactly the columns 0064 rules should have one, and a category keeps its single
    // in-band mechanism rather than acquiring a second.
    let mut presence_written: Vec<&str> = Vec::new();
    for (column, (name, _)) in input.scalar_schema.iter().enumerate() {
        let mut present = croaring::Bitmap::new();
        let mut any_absent = false;
        for (row, item) in items.iter().enumerate() {
            match item.scalars.get(column) {
                Some(ScalarValue::Null) => any_absent = true,
                _ => present.add(row as u32),
            }
        }
        if !any_absent {
            continue;
        }
        if write_render_presence(&seg_dir, name, present, items.len() as u32)?.is_some() {
            presence_written.push(name.as_str());
        }
    }
    // The column itself stays non-nullable (contracts R4) and stores the type's zero; the bitmap
    // above is what makes that zero readable as "nothing" rather than as a value. Substituted
    // here, at the last moment before the writer, so everything upstream still carries the
    // distinction.
    for item in &mut items {
        for (value, (_, ty)) in item.scalars.iter_mut().zip(input.scalar_schema) {
            if matches!(value, ScalarValue::Null) {
                *value = value.or_render_placeholder(*ty);
            }
        }
    }

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
    // it. Reverse: entity → this run's ordinal, dense over the binding rows' span, with the absent
    // sentinel for an entity that has no external id. Both, because the reverse direction is
    // served live-map-first and locator-second, and rotation empties the live map at restart.
    let binding: Vec<&FlushRow> = input
        .rows
        .iter()
        .filter(|r| joins.binary_search(&r.entity_id).is_err())
        .collect();
    let bindings = match (binding.first(), binding.last()) {
        (Some(first), Some(last)) => {
            let (lo, hi) = (first.entity_id.raw(), last.entity_id.raw());
            let mut forward: Vec<(&[u8], u32)> = binding
                .iter()
                .filter_map(|r| {
                    let id = r.external_id.as_deref()?;
                    u32::try_from(r.entity_id.raw()).ok().map(|e| (id, e))
                })
                .collect();
            forward.sort_unstable_by(|a, b| a.0.cmp(b.0));
            write_external_id_run(&seg_dir.join("external-ids.arrow"), &forward)?;
            let mut locator = vec![ROW_ABSENT; (hi - lo + 1) as usize];
            for (ordinal, (_, entity)) in forward.iter().enumerate() {
                locator[(*entity as u64 - lo) as usize] = ordinal as u32;
            }
            write_u32_array(&seg_dir.join("ext-locator.u32"), &locator)?;
            Some(LocatorExtent {
                path: rel("ext-locator.u32"),
                entity_lo: lo,
                entity_hi: hi,
                external_id_run: rel("external-ids.arrow"),
            })
        }
        _ => None,
    };

    // ---- what the manifest must name --------------------------------------------------------
    let mut files = BTreeMap::new();
    let binding_files: &[&str] = match bindings {
        Some(_) => &["external-ids.arrow", "ext-locator.u32"],
        None => &[],
    };
    for name in ["morton.u32", crate::read::CutIndex::FILE, "columns.arrow"]
        .iter()
        .chain(binding_files)
    {
        files.insert(rel(name), digest_of(&seg_dir.join(name))?);
    }
    for column in presence_written {
        let name = format!("{RENDER_PRESENCE_DIR}/{column}.roaring");
        files.insert(rel(&name), digest_of(&seg_dir.join(&name))?);
    }

    Ok(FlushOutput {
        segment: SegmentDescriptor {
            view: view.to_string(),
            incarnation: input.incarnation,
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
        locator_extent: bindings,
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
///
/// **One of [`crate::write::RunWriter`]'s two producers, not a second writer**, on the same
/// argument [`write_flush_segment`] delegates to `SegmentWriter` for: the flush holds its rows
/// anyway, so nothing is streamed *in* here, but this path and the coalesce's k-way merge cannot
/// then drift apart in what `external-ids.arrow` looks like.
pub(crate) fn write_external_id_run(path: &Path, rows: &[(&[u8], u32)]) -> Result<()> {
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut writer = RunWriter::create(path).map_err(io)?;
    for (external_id, entity) in rows {
        writer.append(external_id, *entity).map_err(io)?;
    }
    writer.finish().map_err(io)?;
    Ok(())
}

/// Write one render column's presence bitmap into `segment_dir`, or nothing at all if every row of
/// `0..row_count` is present. Returns the path written.
///
/// **The one writer, for the same reason [`digest_of`] is the one digester**: three render paths
/// produce a segment — the linear build, the streaming build and this flush — and a fourth and
/// fifth rewrite one (the merge and the compaction fold). They agree about where the file goes and
/// what is in it because they all come through here; the format itself is
/// [`crate::render_presence`]'s.
///
/// `present` is run-optimised before serialisation. Absence is the exception in every corpus shape
/// this is for, so the set is long runs of present rows, and run containers are what makes the file
/// small rather than 125 MB per 10⁹ rows.
pub fn write_render_presence(
    segment_dir: &Path,
    column: &str,
    mut present: croaring::Bitmap,
    row_count: u32,
) -> Result<Option<std::path::PathBuf>> {
    present.run_optimize();
    let Some(presence) = RenderPresence::from_present_rows(present, row_count) else {
        return Ok(None);
    };
    let bytes = presence
        .serialise()
        .expect("a bitmap with an absence in it serialises");
    let path = render_presence_path(segment_dir, column);
    let dir = path.parent().expect("the path names a directory");
    fs::create_dir_all(dir).map_err(|source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    fs::write(&path, bytes).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(Some(path))
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

/// How much of a file [`digest_of`] holds at once. One mapping-sized chunk: large enough that the
/// syscall count is irrelevant beside the hash, small enough to be a constant.
const DIGEST_CHUNK_BYTES: usize = 1 << 20;

/// One file's size and hex SHA-256, **streamed in [`DIGEST_CHUNK_BYTES`] chunks**.
///
/// **The buffer is fixed and the file is not, and that is the whole point of this shape.** Every
/// producer of a manifest digests what it just wrote, and for a flush or a merge the operand is
/// bounded — a flush segment is one commit window's rows, a merge's output is capped by
/// `max_merged_segment_bytes`. For a **compaction fold** it is not: pass 5 digests the fold's own
/// `columns.arrow`, which is the whole corpus's columns, tens of GB at 10⁹. Reading that whole —
/// which is what this did, as `fs::read` — puts a corpus-sized `Vec<u8>` in a serving process's
/// heap and is exactly the construction compaction §3 forbids the fold to inherit. **Measured, not
/// supposed**: probe P1 found the fold's peak resident set tracking its own output bytes almost
/// exactly, peaking inside this call rather than in any of the four passes that do the work.
///
/// The size comes from the bytes actually read rather than from `metadata().len()`, so it stays the
/// size of what was hashed even if the file changes underneath — a digest and a length describing
/// different contents is worse than either being wrong.
pub fn digest_of(path: &Path) -> Result<FileDigest> {
    use std::fmt::Write as _;
    use std::io::Read as _;

    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; DIGEST_CHUNK_BYTES];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(FileDigest { size, sha256: hex })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::ColumnsRef;

    const KEY_HEX: &str = "0123456789abcdef0123456789abcdef";

    fn quantisation() -> Quantisation {
        Quantisation {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        }
    }

    fn row(entity: u64, x: f64, score: ScalarValue) -> FlushRow {
        FlushRow {
            entity_id: EntityId::new(entity),
            external_id: None,
            x,
            y: 0.0,
            scalars: vec![score],
        }
    }

    /// Flush `rows` under a `score: i32` schema, and hand back the output and the segment
    /// directory. The rows are laid out along x alone, so the Morton order this sorts into is the
    /// x order and a fixture can predict which row each entity lands at.
    fn flush(dir: &Path, rows: Vec<FlushRow>) -> (FlushOutput, std::path::PathBuf) {
        let key = IdentityKey::from_hex(KEY_HEX).expect("test key");
        let schema = [("score".to_string(), ScalarType::I32)];
        let out = write_flush_segment(
            dir,
            "p",
            "s",
            FlushInput {
                incarnation: 0,
                seg_id: "seg-1",
                rows,
                quantisation: quantisation(),
                identity_key: &key,
                shard_id: 0,
                scalar_schema: &schema,
                row_base: 0,
            }, &[],
        )
        .expect("flush");
        (out, dir.join("partitions/p/views/s/segments/seg-1"))
    }

    /// **The bitmap is in row space, and the fixture is arranged so entity space would be wrong.**
    /// Entity 1 is the one with no score and it sorts to *row 0*, so a bitmap that recorded
    /// entities would call row 1 absent — the same class of error as carrying one across a merge
    /// unpermuted, and in the same fail-open direction for every other row.
    #[test]
    fn a_flushed_segment_records_its_absences_against_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (out, seg_dir) = flush(
            dir.path(),
            vec![
                row(0, 0.9, ScalarValue::I32(7)),
                row(1, 0.1, ScalarValue::Null),
                row(2, 0.5, ScalarValue::I32(9)),
            ],
        );

        assert!(
            out.files
                .contains_key("partitions/p/views/s/segments/seg-1/presence/score.roaring"),
            "the manifest must name the bitmap, or a missing one reads as 'every row present' \
             instead of refusing"
        );

        let columns = ColumnsRef::load(&seg_dir.join("columns.arrow")).expect("columns");
        let presence = columns.presence("score");
        assert!(
            !presence.contains(0),
            "entity 1 has no score and sorts first"
        );
        assert!(presence.contains(1) && presence.contains(2));

        // The column itself is unchanged and still non-nullable: the absent row holds the type's
        // zero, which is exactly the value the bitmap is needed to disambiguate.
        let crate::read::ScalarSlice::I32(scores) = columns.scalar("score").expect("score column")
        else {
            panic!("score is declared i32");
        };
        assert_eq!(scores, [0, 9, 7]);
    }

    /// No absence, no file — and a reader must not be able to tell that from a bitmap with every
    /// bit set, because the common column pays for neither.
    #[test]
    fn a_flush_with_no_absence_writes_no_bitmap_and_still_reads_as_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (out, seg_dir) = flush(
            dir.path(),
            vec![
                row(0, 0.9, ScalarValue::I32(7)),
                row(1, 0.1, ScalarValue::I32(8)),
            ],
        );

        assert!(!seg_dir.join(RENDER_PRESENCE_DIR).exists());
        assert!(!out
            .files
            .keys()
            .any(|rel| rel.contains(RENDER_PRESENCE_DIR)));

        let columns = ColumnsRef::load(&seg_dir.join("columns.arrow")).expect("columns");
        assert!((0..2).all(|row| columns.presence("score").contains(row)));
    }
}
