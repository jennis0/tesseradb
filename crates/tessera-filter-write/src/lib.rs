//! The filter artefact's **write** side: the fold's attribute pass — merging a column's layers into
//! one base — and the derived postings both producers emit through.
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
//! # One postings emit, not two that agree
//!
//! [`write_category_postings`] is called by the batch build and by the fold alike, over whichever
//! source each has: the build walks its staged attribute values, the fold walks the column it has
//! just written. That sharing is §6.2's byte-identity argument — a folded column and a freshly
//! built one over the same live entities are the same bytes — and it is why one crate holds both
//! producers' emit rather than each holding its own.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use croaring::Bitmap;
use tessera_authz::KeyedPostingsSpool;
use tessera_types::SMALL_TERM_THRESHOLD_DEFAULT;

use tessera_filter::{Codes, ColumnKind, ValueColumn, ValueColumnWriter};

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

struct Runs<'a> {
    cursor: croaring::bitmap::BitmapCursor<'a>,
    buf: [croaring::RangeInclusive<u32>; RUN_BUF],
    filled: usize,
    at: usize,
}

impl<'a> Runs<'a> {
    fn new(bitmap: &'a Bitmap) -> Self {
        Runs {
            cursor: bitmap.cursor(),
            buf: [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF],
            filled: 0,
            at: 0,
        }
    }

    /// The next run as `(start, last)`, inclusive.
    fn next(&mut self) -> Option<(u32, u32)> {
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
    let Some(base) = layers.first() else {
        return Err(invalid(
            "the fold's attribute pass was given no layers for a column; a declared column always \
             has at least its base",
        ));
    };
    // **The family comes from the base column, not from the schema.** The fold rewrites what
    // exists, and a kind re-derived from the declaration would silently re-type a column whose file
    // says otherwise; a chunk that disagrees is refused by the writer, which is the second line.
    let kind = ColumnKind::of(base.codes());

    let mut present: Vec<(usize, Bitmap)> = layers
        .iter()
        .enumerate()
        .map(|(i, layer)| (i, layer.present()))
        .collect();
    let mut union = Bitmap::new();
    let mut sum = 0u64;
    for (_, p) in &present {
        sum += p.cardinality();
        union |= p;
    }
    if union.cardinality() != sum {
        return Err(invalid(format!(
            "the fold's attribute pass was given layers claiming one entity twice: {} entities \
             across the layers, {} distinct. Once they collapse into one file the overlap is \
             invisible to the between-layer check for ever, so it is refused here",
            sum,
            union.cardinality()
        )));
    }

    present.retain(|(_, p)| !p.is_empty());
    present.sort_by_key(|(_, p)| p.minimum().expect("the empty layers were dropped"));
    for pair in present.windows(2) {
        let (before, after) = (&pair[0].1, &pair[1].1);
        if before.maximum() >= after.minimum() {
            return Err(invalid(
                "the fold's attribute pass was given interleaved layers: entity ids are issued \
                 monotonically from the high-water (I9), so a layer's entities sit above every \
                 earlier layer's and the merge is linear. Refused rather than sorted, because a \
                 sort here would be papering over a broken allocator",
            ));
        }
    }

    let out_presence = union.andnot(tombstones);
    // Dense from zero to the bound, and nothing looser: `card == bound` with `max == bound - 1`
    // over a set of distinct `u32`s is exactly `[0, bound)`.
    let universal = out_presence.cardinality() == u64::from(bound)
        && (bound == 0 || out_presence.maximum() == Some(bound - 1));

    let mut writer = ValueColumnWriter::create(values_path, presence_path, kind)?;
    for (layer, layer_present) in &present {
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
                writer.push(&slice_codes(codes, at, take))?;
                at += take;
                left -= take;
            }
        }
    }
    writer.finish((!universal).then_some(&out_presence))?;
    Ok(!universal)
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
        // The offsets are `len + 1` — a window carries the end of its last value — and the bytes
        // ride along whole, which the writer's rebasing push is written for.
        Codes::Text { bytes, offsets } => Codes::Text {
            bytes: bytes.clone(),
            offsets: offsets.slice(start, len + 1),
        },
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

    /// **Blanking removes the value bytes**, rather than overwriting them with a sentinel — the
    /// distinction the whole retention argument rests on, asserted against the file itself.
    #[test]
    fn a_blanked_entitys_bytes_are_not_in_the_folded_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base =
            ValueColumn::universal(Codes::text(["alpha", "bravo", "charlie"].map(String::from)));
        let (folded, _) = fold_to_bytes(dir.path(), "text", &[&base], &bitmap([1]), 3);
        assert!(
            !folded.windows(5).any(|w| w == b"bravo"),
            "the blanked value's bytes are still in the column"
        );
        for kept in [&b"alpha"[..], &b"charlie"[..]] {
            assert!(folded.windows(kept.len()).any(|w| w == kept));
        }
        // And the survivors still read back against their own entities, which a shifted offset
        // array would break silently.
        let column = ValueColumn::open(
            &dir.path().join("text-values.arrow"),
            Some(&dir.path().join("text-presence.roaring")),
            false,
        )
        .expect("the folded column opens");
        assert_eq!(column.text_of(0), Some("alpha"));
        assert_eq!(column.text_of(1), None);
        assert_eq!(column.text_of(2), Some("charlie"));
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
