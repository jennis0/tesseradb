//! The filter artefact's **write** side: the fold's attribute pass — merging a column's layers into
//! one base — the coalesce's extent merge beside it, and the derived postings both producers emit
//! through.
//!
//! # Why this is a crate rather than a module of `tessera-filter`
//!
//! **Measured, and it is the seventh time this crate's shape has moved the scan.** `values.rs`
//! carries the scan's inner loop, and the campaign that sized it
//! (`probes/2026-08-08-filter-layout/`) has repeatedly found the constant moving with code that
//! never runs during a scan: `extent.rs` was split out of `values.rs` on exactly that evidence.
//! This code was written there first, and it cost the universal-contiguous arm **0.27 → 0.44 ns**
//! per candidate entity at 10⁹ — a 65% regression — with the hot file byte-identical, and it cost
//! the same whether it sat in a new module or inside `values_writer.rs`. So the boundary that
//! holds is not the file but the **crate**: codegen units are partitioned per crate, and the only
//! way to keep the scan's partitioning a function of the scan's own source is to keep everything
//! else out of the crate. Measured again with the code here: 0.27 ns, within drift of the
//! pre-existing constant.
//!
//! It is also the boundary the layering already wanted. The build and the fold are the two
//! producers, they live in different crates, and neither is on a request path — where
//! `tessera-filter` is opened by every generation and read by every filtered viewport.
//!
//! `filter-index.md` §6.2 is the design. A fold rewrites a bundle, and for this artefact what only
//! it can do is retention — a deleted entity's value bytes leave the corpus here and nowhere else —
//! the postings rebuild to the new watermark, and the collapse of the surviving layers into one
//! base, which is what makes a folded bundle *equal* to a freshly built one rather than close to
//! it.
//!
//! # Blanking is removal from presence, never a sentinel
//!
//! A deleted entity is dropped from the presence bitmap and contributes no value bytes. Writing a
//! reserved value over its slot would keep exactly the bytes retention exists to remove, and no
//! family has a spare value to spend: a category's code 0 is *absent* rather than *deleted*, a
//! string's every byte sequence is a value a corpus may legitimately hold, and a numeric's every
//! bit pattern is one. So a previously universal column becomes partial at its first folded
//! deletion — which moves it from §2.1's bare-array addressing to the presence-addressed one, both
//! measured and both inside budget.
//!
//! # Why the merge is linear, and what it refuses
//!
//! The layers partition entity space and each is entity-ascending, and a flush's entities are ids
//! issued above every earlier layer's (**I9**) — so the concatenation is a linear merge with no
//! sort. Both halves of that premise are *checked* rather than assumed, because neither has a
//! symptom if it fails: values would be paired with the wrong entities from the first violation
//! onwards, and every later filter would answer confidently and wrongly.
//!
//! **The duplicate-entity refusal is this pass's own, and it cannot be borrowed from composition.**
//! `FilterColumns::compose` tests disjointness *between* layers; once those layers collapse into
//! one file an overlap among the inputs is internal to a single layer and invisible to that check
//! for ever. So the merge checks the union's cardinality against the sum of its inputs' — O(the
//! containers touched), before a value is written.
//!
//! # Two producers of one merge: the fold, and the coalesce
//!
//! [`fold_value_column`] and [`coalesce_attr_extents`] are the same linear merge under two
//! different obligations, so they share it rather than each stating the checks above (§5.2's
//! "attribute extents are the same shape as delta tiers, and take the same safety argument").
//! [`fold_keyword_column`] and [`coalesce_keyword_extents`] are the same two producers again, over
//! a family whose values are ordinals into a per-layer dictionary; they reach the merge through the
//! same order and the same guards, and add a remap of their own that the `keyword` module argues
//! for. What differs is small and worth naming, because getting either wrong is silent:
//!
//! - The fold **retires**: `D₀`'s entities are blanked. A coalesce **retires nothing** — removal is
//!   Rule F's, and a deleted-but-unfolded entity's value rides through untouched (§5.2, §6).
//! - The fold may write **no** presence bitmap, which is how a base column says "the entity id is
//!   the array index". A coalesced extent always writes one: its entities start above the build's
//!   high-water and need not be contiguous, so a positional read would pair every value with the
//!   wrong entity (§2.5). The one file whose absence carries meaning is never absent here.
//!
//! # One postings emit, not two that agree
//!
//! [`write_category_postings`] is called by the batch build and by the fold alike, over whichever
//! source each has: the build walks its staged attribute values, the fold walks the column it has
//! just written. That sharing is §6.2's byte-identity argument — a folded column and a freshly
//! built one over the same live entities are the same bytes — and it is why one crate holds both
//! producers' emit rather than each holding its own.

mod keyword;
mod record;
mod text;

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use arrow::buffer::ScalarBuffer;
use croaring::Bitmap;
use tessera_authz::KeyedPostingsSpool;
use tessera_types::SMALL_TERM_THRESHOLD_DEFAULT;

use tessera_filter::{Codes, ColumnKind, ValueColumn, ValueColumnWriter};

pub use keyword::{coalesce_keyword_extents, fold_keyword_column, KeywordLayer};
pub use record::{
    coalesce_record_extents, fold_record_blob, merge_record_rows, BlobRows, BlockPool,
    RecordBlobWriter, RecordRows,
};
pub use text::{coalesce_text_extents, merge_text_layers, TextLayerRef};

/// The vocabulary's reserved *absent* code: never drawn, never bound to a key, and carried by
/// exactly the entities that carry no value.
///
/// Duplicated from `tessera_store::vocabulary::ABSENT_CODE` rather than imported, because
/// `check-layers.sh` denies this crate the edge to `tessera-store` (filter-index §9) — the value is
/// part of the artefact's format, which this crate is on the writing end of.
const ABSENT_CODE: u32 = 0;

/// Values per push into the streaming writer. A candidate run may be the whole column, and a text
/// column's push writes the run's bytes in one call — so the chunk bounds the write rather than the
/// memory, which the writer's own spool already bounds.
const MERGE_CHUNK: usize = 1 << 20;

/// Entity ids the postings emit holds in flight, at 4 B each: 2²⁶ is a 256 MB flat buffer, and the
/// cost of a smaller band is one more column scan — measured at ~280 ms per 10⁹ (filter-index
/// §2.2), so seconds per column even at sixteen bands.
pub const POSTINGS_BAND_ROWS: usize = 1 << 26;

/// Runs read from a bitmap in bulk, `RUN_BUF` at a time.
///
/// **A deliberate duplicate of `tessera-filter`'s own run iterator**, and the duplication is the
/// point of this crate: sharing it would mean exporting it, and every export is a reason for the
/// scan's crate to hold code the scan does not run (see the module doc). It is twenty lines over
/// croaring's public cursor.
const RUN_BUF: usize = 64;

pub(crate) struct Runs<'a> {
    cursor: croaring::bitmap::BitmapCursor<'a>,
    buf: [croaring::RangeInclusive<u32>; RUN_BUF],
    filled: usize,
    at: usize,
}

impl<'a> Runs<'a> {
    pub(crate) fn new(bitmap: &'a Bitmap) -> Self {
        Runs {
            cursor: bitmap.cursor(),
            buf: [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF],
            filled: 0,
            at: 0,
        }
    }

    /// The next run as `(start, last)`, inclusive.
    pub(crate) fn next(&mut self) -> Option<(u32, u32)> {
        if self.at == self.filled {
            self.filled = self.cursor.read_many_ranges(&mut self.buf);
            self.at = 0;
            if self.filled == 0 {
                return None;
            }
        }
        let r = self.buf[self.at];
        self.at += 1;
        Some((r.start, r.last))
    }
}

/// The vocabulary code at `slot`, for the three widths a category is stored at.
///
/// `None` for every other family — and that is a refusal rather than a default, because the value
/// a wider or signed column holds is not a code and a posting keyed on it would name a value
/// nothing carries.
fn code_at(codes: &Codes, slot: usize) -> Option<u32> {
    match codes {
        Codes::U8(v) => Some(u32::from(v[slot])),
        Codes::U16(v) => Some(u32::from(v[slot])),
        Codes::U32(v) => Some(v[slot]),
        _ => None,
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// One column's layers merged into one base, with `tombstones` blanked.
///
/// `layers` is the base column followed by every extent the fold consumes, in any order — the merge
/// sorts them by their own entity ranges and refuses an interleaving. `bound` is one past the
/// highest entity the fold's snapshot covers: the presence bitmap is **omitted** only when every
/// entity from 0 to that bound is present, which is the reader's dense-from-zero convention ("the
/// entity id is the array index", §2.1) and nothing looser.
///
/// Returns whether a presence bitmap was written, so the caller can name exactly the files that
/// exist in the manifest.
pub fn fold_value_column(
    layers: &[&ValueColumn],
    tombstones: &Bitmap,
    bound: u32,
    values_path: &Path,
    presence_path: &Path,
) -> io::Result<bool> {
    let (order, out_presence) = merge_order(layers, tombstones, "the fold's attribute pass")?;
    // Dense from zero to the bound, and nothing looser: `card == bound` with `max == bound - 1`
    // over a set of distinct `u32`s is exactly `[0, bound)`.
    let universal = out_presence.cardinality() == u64::from(bound)
        && (bound == 0 || out_presence.maximum() == Some(bound - 1));
    write_merged(
        layers,
        &order,
        tombstones,
        values_path,
        presence_path,
        (!universal).then_some(&out_presence),
        None,
    )?;
    Ok(!universal)
}

/// One column's extents merged into one, for the entity-space coalesce (`filter-index.md` §5.2).
///
/// `inputs` is a window of one column's own extents, in any order — the merge sorts them by their
/// entity ranges and refuses an interleaving, exactly as the fold's merge does. What is written is
/// the same `(entity, value)` relation the inputs carried between them, in one values file and one
/// presence bitmap: layers are unioned at composition, so their division into files is immaterial
/// and the pass is a **content-preserving re-encode**.
///
/// **A coalesce retires nothing**, so there is no tombstone parameter to pass and no way to spell
/// one: a deleted-but-unfolded entity's value rides through untouched, because removal is the
/// fold's (Rule F, write-path §5.4). An extent's presence bitmap is always written, for the reason
/// [`tessera_filter::write_extent`] gives.
pub fn coalesce_attr_extents(
    inputs: &[&ValueColumn],
    values_path: &Path,
    presence_path: &Path,
) -> io::Result<()> {
    if inputs.len() < 2 {
        return Err(invalid(format!(
            "the attribute coalesce was given {} extents; it collapses a window of a column's \
             extents into one and there is nothing to collapse below two",
            inputs.len()
        )));
    }
    let (order, presence) = merge_order(inputs, &Bitmap::new(), "the attribute coalesce")?;
    write_merged(
        inputs,
        &order,
        &Bitmap::new(),
        values_path,
        presence_path,
        Some(&presence),
        None,
    )
}

/// The layers in the order the linear merge reads them, and the presence the merged column carries.
///
/// Both refusals live here rather than at either caller, and neither has a symptom if it is
/// skipped — the values would be paired with the wrong entities from the first violation onwards,
/// and every later filter would answer confidently and wrongly.
///
/// The empty layers are dropped rather than ordered: an extent for a column no flushed entity
/// carried a value in is written anyway, so the file set is a function of the schema (§2.5), and
/// such a layer has no entity range to sort by.
pub(crate) fn merge_order(
    layers: &[&ValueColumn],
    tombstones: &Bitmap,
    pass: &str,
) -> io::Result<(Vec<(usize, Bitmap)>, Bitmap)> {
    if layers.is_empty() {
        return Err(invalid(format!(
            "{pass} was given no layers for a column; a declared column always has at least its \
             base"
        )));
    }
    let present: Vec<(usize, Bitmap)> = layers
        .iter()
        .enumerate()
        .map(|(i, layer)| (i, layer.present()))
        .collect();
    let (present, union) = ordered_disjoint(present, pass)?;
    Ok((present, union.andnot(tombstones)))
}

/// The two refusals both merge axes rest on — value columns and the record blob alike — over the
/// layers' entity sets alone: no layer claims an entity another holds, and the layers do not
/// interleave, so the concatenation in sorted order is a linear merge. Returns the non-empty
/// layers in merge order and the union of every layer's entities.
///
/// Shared rather than restated because neither refusal has a symptom if one copy drifts: values
/// (or rows) would be paired with the wrong entities from the first violation onwards.
pub(crate) fn ordered_disjoint(
    mut present: Vec<(usize, Bitmap)>,
    pass: &str,
) -> io::Result<(Vec<(usize, Bitmap)>, Bitmap)> {
    let mut union = Bitmap::new();
    let mut sum = 0u64;
    for (_, p) in &present {
        sum += p.cardinality();
        union |= p;
    }
    if union.cardinality() != sum {
        return Err(invalid(format!(
            "{pass} was given layers claiming one entity twice: {} entities across the layers, {} \
             distinct. Once they collapse into one file the overlap is invisible to the \
             between-layer check for ever, so it is refused here",
            sum,
            union.cardinality()
        )));
    }

    present.retain(|(_, p)| !p.is_empty());
    present.sort_by_key(|(_, p)| p.minimum().expect("the empty layers were dropped"));
    for pair in present.windows(2) {
        let (before, after) = (&pair[0].1, &pair[1].1);
        if before.maximum() >= after.minimum() {
            return Err(invalid(format!(
                "{pass} was given interleaved layers: entity ids are issued monotonically from the \
                 high-water (I9), so a layer's entities sit above every earlier layer's and the \
                 merge is linear. Refused rather than sorted, because a sort here would be papering \
                 over a broken allocator"
            )));
        }
    }
    Ok((present, union))
}

/// Stream the ordered layers into one column, skipping `tombstones`.
///
/// `presence` is the file the reader will address by, or `None` where the caller's convention lets
/// it be omitted — which is a base column dense from zero, and never an extent.
///
/// `remap` is the keyword family's `old ordinal -> new ordinal` table per layer, and its presence
/// is what separates the two write paths: without it every value is pushed as a **borrowed slice**
/// of the layer's own buffers, so the bytes cannot change; with it every value is rewritten. That
/// difference is the whole reason the `keyword` module carries a content guard the checks above
/// cannot supply, and the guard runs before this function is called.
pub(crate) fn write_merged(
    layers: &[&ValueColumn],
    order: &[(usize, Bitmap)],
    tombstones: &Bitmap,
    values_path: &Path,
    presence_path: &Path,
    presence: Option<&Bitmap>,
    remap: Option<&[Vec<u32>]>,
) -> io::Result<()> {
    // **The family comes from the first layer, not from the schema.** Both passes rewrite what
    // exists, and a kind re-derived from the declaration would silently re-type a column whose file
    // says otherwise; a chunk that disagrees is refused by the writer, which is the second line.
    let kind = ColumnKind::of(layers[0].codes());
    let mut writer = ValueColumnWriter::create(values_path, presence_path, kind)?;
    for (layer, layer_present) in order {
        let codes = layers[*layer].codes();
        // The kept entities of this layer. A run of them is contiguous in entity space *and* in
        // slot space — every entity in it is present in this layer, so its slots are consecutive —
        // which is what lets the merge push a borrowed slice of the layer's values rather than
        // walking it a value at a time.
        let keep = layer_present.andnot(tombstones);
        let mut runs = Runs::new(&keep);
        while let Some((start, last)) = runs.next() {
            let slot0 = (layer_present.rank(start) - 1) as usize;
            let mut at = slot0;
            let mut left = (last - start) as usize + 1;
            while left > 0 {
                let take = left.min(MERGE_CHUNK);
                match remap {
                    None => writer.push(&slice_codes(codes, at, take))?,
                    Some(tables) => writer.push(&recolour(codes, at, take, &tables[*layer])?)?,
                }
                at += take;
                left -= take;
            }
        }
    }
    writer.finish(presence)
}

/// `len` ordinals from `start`, each rewritten through this layer's remap.
///
/// The one place in either pass where a merged value is **built** rather than borrowed, which is
/// why the two refusals here are worth their cost per chunk. An ordinal past the end of the remap
/// is a column paired with a dictionary that never coloured it; an ordinal mapping to
/// [`keyword::NO_KEY`] is an entity still reaching a key the rebuild found no survivor for. Both
/// are contradictions, and both would otherwise be published as some other key.
fn recolour(codes: &Codes, start: usize, len: usize, remap: &[u32]) -> io::Result<Codes> {
    let Codes::U32(src) = codes else {
        return Err(invalid(format!(
            "a remapped merge was given a {:?} column; only a keyword layer's u32 ordinals are \
             rewritten",
            ColumnKind::of(codes)
        )));
    };
    let mut out = Vec::with_capacity(len);
    for &ordinal in &src[start..start + len] {
        let new = *remap.get(ordinal as usize).ok_or_else(|| {
            invalid(format!(
                "a remapped merge met ordinal {ordinal} in a layer whose dictionary holds {} keys \
                 — the column and the dictionary are not the same layer's",
                remap.len()
            ))
        })?;
        if new == keyword::NO_KEY {
            return Err(invalid(format!(
                "a remapped merge met ordinal {ordinal}, whose key the rebuild dropped as carried \
                 by no surviving entity — yet an entity carries it"
            )));
        }
        out.push(new);
    }
    Ok(Codes::U32(ScalarBuffer::from(out)))
}

/// `len` values from `start`, borrowed rather than copied: every arm is a window onto the layer's
/// own buffers, which for a mapped column is a window onto the file.
fn slice_codes(codes: &Codes, start: usize, len: usize) -> Codes {
    match codes {
        Codes::U8(v) => Codes::U8(v.slice(start, len)),
        Codes::U16(v) => Codes::U16(v.slice(start, len)),
        Codes::U32(v) => Codes::U32(v.slice(start, len)),
        Codes::U64(v) => Codes::U64(v.slice(start, len)),
        Codes::I8(v) => Codes::I8(v.slice(start, len)),
        Codes::I16(v) => Codes::I16(v.slice(start, len)),
        Codes::I32(v) => Codes::I32(v.slice(start, len)),
        Codes::I64(v) => Codes::I64(v.slice(start, len)),
        Codes::F32(v) => Codes::F32(v.slice(start, len)),
        Codes::F64(v) => Codes::F64(v.slice(start, len)),
    }
}

/// A source of `(entity, code)` pairs in ascending entity order, scannable more than once.
///
/// **Two producers, one emit.** The build's source is its staged attribute values; the fold's is
/// the column it has just written. The trait exists so that the banded emit below is written once —
/// filter-index §6.2 makes that byte-identity rather than a tidiness preference — and it is scanned
/// twice, once to count and once per band, which is why it takes `&self` rather than being an
/// iterator.
pub trait CategorySource {
    /// Visit every entity carrying a value, ascending, with its code. The **absent** code is not a
    /// value and must not be visited.
    fn for_each(&self, f: &mut dyn FnMut(u32, u32) -> io::Result<()>) -> io::Result<()>;
}

/// A folded (or built) category column, read back as its own postings' source — which is what makes
/// the accelerator a *derivative* of the value column rather than a second producer that agrees.
impl CategorySource for ValueColumn {
    fn for_each(&self, f: &mut dyn FnMut(u32, u32) -> io::Result<()>) -> io::Result<()> {
        match self.codes() {
            Codes::U8(_) | Codes::U16(_) | Codes::U32(_) => {}
            // Only a category has a code, and a category is one of the three widths above.
            // `Codes::at` answers `u32::MAX` for everything else, which would key a posting on a
            // value nothing carries — refused rather than emitted.
            _ => {
                return Err(invalid(
                    "postings were asked for over a column that is not a category; only a \
                     category's values carry an identity a posting can be keyed by",
                ))
            }
        }
        let present = self.present();
        let mut runs = Runs::new(&present);
        while let Some((start, last)) = runs.next() {
            let slot0 = (present.rank(start) - 1) as usize;
            for (k, entity) in (start..=last).enumerate() {
                let code = code_at(self.codes(), slot0 + k)
                    .expect("the family was checked before the walk began");
                if code == ABSENT_CODE {
                    continue;
                }
                f(entity, code)?;
            }
        }
        Ok(())
    }
}

/// One category column's derived postings, emitted band by band.
///
/// **The counting pass is what makes the scatter possible without holding the relation.** It gives
/// each code's exact member count, which lays a band's flat buffer out by prefix sums — so the emit
/// pass writes each entity to a known slot rather than growing a list per code. Its residue is one
/// count per distinct code: vocabulary-sized by definition (filter-index §2.3), kilobytes.
///
/// **A code is never split across a band**, which is what lets a band be encoded and appended the
/// moment its scatter finishes; a code larger than the budget forms a band of its own, exactly as
/// the authorisation build's `band_rows_budget` takes its floor from the largest single term. Bands
/// partition ascending code space, so [`KeyedPostingsSpool`]'s ascending-key check holds across
/// them unchanged.
///
/// The two fail-closed scatter checks are the authorisation emit's, for its reasons: an overflow
/// check, because one code's entities silently becoming another's is a disclosure; and a short-fill
/// check **by count rather than by value**, because zero is a valid entity id.
///
/// A code with no members at all is dropped rather than written empty: the positional format has no
/// such choice — every ordinal below the largest must exist — but a keyed file addresses by search,
/// and a missing key already reads as the empty set.
pub fn write_category_postings(
    path: &Path,
    column: &str,
    source: &dyn CategorySource,
    band_rows: usize,
) -> io::Result<()> {
    let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
    source.for_each(&mut |_entity, code| {
        let slot = counts.entry(code).or_insert(0);
        *slot = slot.checked_add(1).ok_or_else(|| {
            invalid(format!(
                "attribute '{column}': code {code} is carried by more than 2^32 entities, which \
                 the entity ceiling makes impossible"
            ))
        })?;
        Ok(())
    })?;
    let codes: Vec<u32> = counts.keys().copied().collect();
    let rows: Vec<u32> = counts.values().copied().collect();
    drop(counts);

    let budget = band_rows
        .max(rows.iter().copied().max().unwrap_or(0) as usize)
        .max(1);
    let mut bands: Vec<(usize, usize)> = Vec::new();
    let mut lo = 0usize;
    let mut acc = 0usize;
    for (i, &r) in rows.iter().enumerate() {
        if acc + r as usize > budget && i > lo {
            bands.push((lo, i));
            lo = i;
            acc = 0;
        }
        acc += r as usize;
    }
    bands.push((lo, codes.len()));

    let spool_path = path.with_extension("spool");
    let mut spool = KeyedPostingsSpool::create(&spool_path, SMALL_TERM_THRESHOLD_DEFAULT)?;
    for (lo, hi) in bands {
        let keys = &codes[lo..hi];
        let width = hi - lo;
        let mut offsets: Vec<u64> = Vec::with_capacity(width + 1);
        let mut total = 0u64;
        offsets.push(0);
        for &r in &rows[lo..hi] {
            total += u64::from(r);
            offsets.push(total);
        }
        let mut flat: Vec<u32> = vec![0; total as usize];
        let mut cursor: Vec<u64> = offsets[..width].to_vec();
        // Entities are swept ascending, so each posting's entity list arrives sorted and
        // `encode_posting`'s unconditional sortedness check re-verifies the property this loop
        // established rather than taking it on trust.
        source.for_each(&mut |entity, code| {
            // A code outside this band is this band's business only in that it is not.
            let Ok(local) = keys.binary_search(&code) else {
                return Ok(());
            };
            let slot = &mut cursor[local];
            if *slot >= offsets[local + 1] {
                return Err(invalid(format!(
                    "attribute '{column}': code {code} received more postings than the {} the \
                     counting pass found",
                    offsets[local + 1] - offsets[local]
                )));
            }
            flat[*slot as usize] = entity;
            *slot += 1;
            Ok(())
        })?;
        for (local, slot) in cursor.iter().enumerate() {
            if *slot != offsets[local + 1] {
                return Err(invalid(format!(
                    "attribute '{column}': code {} expected {} postings, received {}",
                    keys[local],
                    offsets[local + 1] - offsets[local],
                    slot - offsets[local]
                )));
            }
        }
        for local in 0..width {
            let entities = &flat[offsets[local] as usize..offsets[local + 1] as usize];
            spool.append(keys[local], entities)?;
        }
    }
    spool.finish(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::buffer::ScalarBuffer;
    use tessera_filter::write_value_column;

    fn codes_u32(v: Vec<u32>) -> Codes {
        Codes::U32(ScalarBuffer::from(v))
    }

    fn bitmap(entities: impl IntoIterator<Item = u32>) -> Bitmap {
        let mut b = Bitmap::new();
        for e in entities {
            b.add(e);
        }
        b
    }

    /// Fold `layers`, and return the two files' bytes (the presence file's `None` where none was
    /// written).
    fn fold_to_bytes(
        dir: &Path,
        tag: &str,
        layers: &[&ValueColumn],
        tombstones: &Bitmap,
        bound: u32,
    ) -> (Vec<u8>, Option<Vec<u8>>) {
        let values = dir.join(format!("{tag}-values.arrow"));
        let presence = dir.join(format!("{tag}-presence.roaring"));
        let partial =
            fold_value_column(layers, tombstones, bound, &values, &presence).expect("the fold");
        (
            std::fs::read(&values).expect("values"),
            partial.then(|| std::fs::read(&presence).expect("presence")),
        )
    }

    /// **A folded column is byte-identical to a freshly built one over the same live entities.**
    ///
    /// filter-index §6.2's equivalence claim is byte-identity rather than a tolerance, and it holds
    /// only because both producers emit through the one writer. Asserted over both the universal
    /// shape — where the presence file's *absence* is the meaning — and a partial one.
    #[test]
    fn a_folded_column_is_the_bytes_a_single_build_would_have_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A base covering [0, 6) and two extents above it, out of listed order.
        let base = ValueColumn::universal(codes_u32((0..6).map(|e| e * 10).collect()));
        let first = ValueColumn::partial(codes_u32(vec![60, 70]), bitmap([6, 7])).expect("extent");
        let second = ValueColumn::partial(codes_u32(vec![80, 90]), bitmap([8, 9])).expect("extent");

        let (folded, presence) = fold_to_bytes(
            dir.path(),
            "universal",
            &[&base, &second, &first],
            &Bitmap::new(),
            10,
        );
        assert!(
            presence.is_none(),
            "dense from zero to the bound, so the presence file's absence carries the meaning"
        );
        let built = dir.path().join("built.arrow");
        write_value_column(
            &built,
            &dir.path().join("built.roaring"),
            &codes_u32((0..10).map(|e| e * 10).collect()),
            None,
        )
        .expect("the one-shot writer");
        assert_eq!(folded, std::fs::read(&built).expect("built"));

        // And with one entity blanked: the same bytes a build over the survivors alone would
        // write, which is what the partial path has to reach too.
        let tombstones = bitmap([3]);
        let (folded, presence) = fold_to_bytes(
            dir.path(),
            "blanked",
            &[&base, &first, &second],
            &tombstones,
            10,
        );
        let survivors: Vec<u32> = (0..10).filter(|e| *e != 3).map(|e| e * 10).collect();
        let live = bitmap((0..10u32).filter(|e| *e != 3));
        let built = dir.path().join("built-partial.arrow");
        let built_presence = dir.path().join("built-partial.roaring");
        write_value_column(&built, &built_presence, &codes_u32(survivors), Some(&live))
            .expect("the one-shot writer");
        assert_eq!(folded, std::fs::read(&built).expect("built"));
        assert_eq!(
            presence.expect("a blanked column is partial"),
            std::fs::read(&built_presence).expect("built presence")
        );
    }

    /// **The duplicate-entity refusal is the merge's own**, because after the merge there is one
    /// layer and the between-layer disjointness check can never see the overlap again.
    #[test]
    fn two_layers_claiming_one_entity_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = ValueColumn::universal(codes_u32(vec![1, 2, 3]));
        let overlapping = ValueColumn::partial(codes_u32(vec![9]), bitmap([2])).expect("an extent");
        let err = fold_value_column(
            &[&base, &overlapping],
            &Bitmap::new(),
            3,
            &dir.path().join("v.arrow"),
            &dir.path().join("p.roaring"),
        )
        .expect_err("an overlap is refused");
        assert!(err.to_string().contains("twice"), "{err}");
    }

    /// Coalesce `inputs` into one extent and open it back.
    fn coalesce_to(dir: &Path, tag: &str, inputs: &[&ValueColumn]) -> io::Result<ValueColumn> {
        let values = dir.join(format!("{tag}-values.arrow"));
        let presence = dir.join(format!("{tag}-presence.roaring"));
        coalesce_attr_extents(inputs, &values, &presence)?;
        ValueColumn::open(&values, Some(&presence), tessera_filter::Access::Read)
    }

    /// **A coalesced extent carries exactly the `(entity, value)` triples its inputs carried
    /// between them**, which is the whole of §5.2's content-preserving claim — asserted over a
    /// *second* coalesce of the first's output, which is the recursion the per-column selection
    /// unit is what makes free. The keyword family's own version of this claim, where the merge
    /// additionally renumbers every ordinal, is in `keyword.rs`.
    #[test]
    fn a_coalesced_extent_carries_its_inputs_triples_and_coalesces_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Four extents, disjoint and ascending, with a gap between each — the shape a flush
        // publishes, where the ids are issued from a high-water other views also draw on.
        let extent = |base: u32| {
            let entities: Vec<u32> = (0..3).map(|k| base + k * 2).collect();
            ValueColumn::partial(codes_u32(entities.clone()), bitmap(entities)).expect("an extent")
        };
        let extents: Vec<ValueColumn> = [100u32, 200, 300, 400].into_iter().map(extent).collect();
        let refs: Vec<&ValueColumn> = extents.iter().collect();

        let first = coalesce_to(dir.path(), "first", &refs[..2]).expect("the coalesce");
        let second = coalesce_to(dir.path(), "second", &refs[2..]).expect("the coalesce");
        // The recursion: a coalesced extent is an extent like any other, and the next rung takes
        // it identically.
        let again = coalesce_to(dir.path(), "again", &[&first, &second]).expect("the recursion");

        for column in [&first, &second, &again] {
            for entity in column.present().iter() {
                assert_eq!(
                    column.value_of(entity).map(|v| v.raw()),
                    Some(entity),
                    "entity {entity} reads back another entity's value"
                );
            }
        }
        let expected: Vec<u32> = extents
            .iter()
            .flat_map(|e| e.present().iter().collect::<Vec<_>>())
            .collect();
        assert_eq!(again.present().iter().collect::<Vec<_>>(), expected);
        assert_eq!(
            first.present().or(&second.present()),
            again.present(),
            "the coalesced presence is the union of its inputs', which is what the replace \
             composition then checks against"
        );
    }

    /// **A coalesce retires nothing** (§5.2, §6): removal is the fold's, and a deleted entity is
    /// still in the overlay rather than in any artefact — so its value must ride through untouched.
    /// A pass that blanked here would be a third retirement route, which is how Rule S and Rule F
    /// get conflated.
    ///
    /// **Fault injected:** pass a tombstone set through to `write_merged` from
    /// `coalesce_attr_extents` and this fails on the missing entity.
    #[test]
    fn a_coalesce_carries_every_entitys_value_through_including_a_deleted_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first =
            ValueColumn::partial(codes_u32(vec![10, 11]), bitmap([100, 101])).expect("an extent");
        // Entity 200 is deleted-but-unfolded: it is in the overlay's `deleted` set, and no
        // attribute artefact knows that or may act on it.
        let second =
            ValueColumn::partial(codes_u32(vec![20, 21]), bitmap([200, 201])).expect("an extent");
        let out = coalesce_to(dir.path(), "kept", &[&first, &second]).expect("the coalesce");
        assert_eq!(
            out.present().iter().collect::<Vec<_>>(),
            vec![100, 101, 200, 201]
        );
        assert_eq!(out.value_of(200).map(|c| c.raw()), Some(20));
    }

    /// **The duplicate-entity refusal is the coalesce's own too**, for the fold's reason: after the
    /// merge the eight inputs are one layer, and `FilterColumns::compose`'s between-layer check can
    /// never see the overlap again — so an I9 violation would be laundered into a clean-looking
    /// artefact.
    ///
    /// **Fault injected:** drop the cardinality comparison in `merge_order` and this writes a file
    /// holding one entity twice rather than refusing.
    #[test]
    fn two_extents_claiming_one_entity_are_refused_by_the_coalesce() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = ValueColumn::partial(codes_u32(vec![1, 2]), bitmap([7, 8])).expect("an extent");
        let clash = ValueColumn::partial(codes_u32(vec![9]), bitmap([8])).expect("an extent");
        let err =
            coalesce_to(dir.path(), "clash", &[&first, &clash]).expect_err("an overlap is refused");
        assert!(err.to_string().contains("twice"), "{err}");
        // And a single extent is not a window: the pass collapses several into one.
        assert!(coalesce_to(dir.path(), "alone", &[&first]).is_err());
    }

    /// **Interleaved layers are refused rather than sorted.** The merge is linear because entity
    /// ids are issued monotonically (**I9**); a sort here would paper over an allocator that had
    /// stopped doing that, and the symptom would be values paired with the wrong entities.
    #[test]
    fn interleaved_layers_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let odd = ValueColumn::partial(codes_u32(vec![1, 3]), bitmap([1, 3])).expect("a layer");
        let even = ValueColumn::partial(codes_u32(vec![0, 2]), bitmap([0, 2])).expect("a layer");
        let err = fold_value_column(
            &[&odd, &even],
            &Bitmap::new(),
            4,
            &dir.path().join("v.arrow"),
            &dir.path().join("p.roaring"),
        )
        .expect_err("interleaving is refused");
        assert!(err.to_string().contains("interleaved"), "{err}");
    }

    /// **The postings are what the folded column says**, code for code — which is what makes one a
    /// derivative of the other rather than two writers that happen to agree.
    #[test]
    fn the_rebuilt_postings_agree_with_the_folded_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Code 0 is *absent* and must earn no posting; the rest are scattered as a vocabulary mints
        // them.
        let values: Vec<u32> = (0..200u32)
            .map(|e| match e % 5 {
                0 => ABSENT_CODE,
                1 => 3_999_999_999,
                2 => 17,
                _ => 1_000 + e % 7,
            })
            .collect();
        let present = bitmap((0..200u32).filter(|e| values[*e as usize] != ABSENT_CODE));
        let kept: Vec<u32> = values
            .iter()
            .copied()
            .filter(|c| *c != ABSENT_CODE)
            .collect();
        let column = ValueColumn::partial(codes_u32(kept), present).expect("a column");

        let path = dir.path().join("postings.arrow");
        write_category_postings(&path, "colour", &column, POSTINGS_BAND_ROWS).expect("the emit");
        let postings = tessera_filter::ColumnPostings::open_keyed(&path).expect("open");
        let mut codes: Vec<u32> = values
            .iter()
            .copied()
            .filter(|c| *c != ABSENT_CODE)
            .collect();
        codes.sort_unstable();
        codes.dedup();
        for code in codes {
            let members = postings
                .entities(tessera_types::AttrLocalId::new(code))
                .expect("read");
            let expected: Vec<u32> = (0..200u32)
                .filter(|e| values[*e as usize] == code)
                .collect();
            assert_eq!(members.iter().collect::<Vec<_>>(), expected, "code {code}");
        }
        assert!(
            postings
                .entities(tessera_types::AttrLocalId::new(ABSENT_CODE))
                .expect("read")
                .is_empty(),
            "the absent code is not a value and earns no posting"
        );

        // Banding is a memory knob, not a format one: the same bytes at every band size.
        let narrow = dir.path().join("postings-narrow.arrow");
        write_category_postings(&narrow, "colour", &column, 1).expect("the emit");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            std::fs::read(&narrow).expect("read")
        );
    }
}
