//! The keyword family's two lifecycle transformations: the coalesce's dictionary merge with its
//! ordinal remap, and the fold's rebuild of the dictionary from the values that survive
//! (`records-and-search.md` §7; `filter-index.md` §5.2 for the coalesce, §6.2 for the fold).
//!
//! ⊘ **Nothing calls either pass yet**, and there are two separate reasons rather than one. No
//! producer writes a keyword layer: the base build and the flush that will are a parallel track's,
//! so both passes are exercised by this module's tests over layers built in process from the two
//! libraries they consume. And the engine's coalesce declines a column whose layers carry
//! dictionaries — not for want of this merge but because the live generation's composition carries
//! a layer's *values* alone, so a coalesced extent could not swap in beside its own dictionary
//! (`crate::coalesce_attr_extents`'s caller states it at the selection). Until that seam exists a
//! keyword column's extents wait for the fold, which is a bounded steady state and not a leak.
//!
//! # This merge changes the bytes, and that is a different correctness shape
//!
//! The shipped attribute coalesce ([`crate::coalesce_attr_extents`]) concatenates windows of
//! extents with values **byte-preserved**: it pushes borrowed slices of each input through
//! unchanged. That is precisely why its guards suffice. Union-cardinality-equals-sum over presence
//! and the non-interleaving order check bound everything that can go wrong when the bytes do not
//! change — which entity owns which slot, and nothing else.
//!
//! A keyword coalesce **renumbers**. Each input's dictionary is merged into one, a remap
//! (`old ordinal -> new ordinal`) is built per input, and every ordinal is rewritten through it.
//! The inherited guards cannot see a wrong remap, because **recolouring every value changes no
//! cardinality**: presence is untouched, the union still equals the sum, the layers still do not
//! interleave, and the published extent answers confidently with another value's key. So this
//! merge carries its own content guard, discharged in [`verify_remap`] before any ordinal is
//! written:
//!
//! - the remap is **strictly monotone** — both dictionaries are sorted, so a correct remap is —
//!   and every entry lands inside the merged dictionary;
//! - `merged_dict[remap[i]] == input_dict[i]` for **every** input key, O(keys);
//! - **every merged key is claimed by an input**, which is the same statement read the other way
//!   and is what makes the fold's retention argument checkable (below).
//!
//! A mismatch refuses the pass rather than publishing it. The test that proves the guard fires is
//! `a_monotone_but_wrong_remap_is_refused_where_the_shipped_guards_are_silent`, which corrupts a
//! remap into one that is monotone, in range, and wrong, then shows the inherited guards passing
//! over the same inputs and the recoloured column reading back another entity's key.
//!
//! **`tessera-authz`'s `coalesce_dict_extents` is not a template for this**, and the design's
//! "same shape" reading of it was a review finding for exactly this reason: that merge is
//! ordinal-*preserving* by construction, so it owes no content guard at all. Borrowing its
//! assumptions here is the defect the guard exists to catch.
//!
//! # The fold rebuilds the dictionary from what survives, and a key can leave
//!
//! The fold streams base plus extents in entity order, skips the blanked set, and rebuilds the
//! dictionary **whole from the values the surviving entities carry**. A key whose only carrier was
//! blanked is in no layer's live ordinal set, so it is never pushed and its bytes are not in the
//! folded artefact — the retention argument (index §6.2) reaching dictionary keys, which is the one
//! thing this family owes beyond the ordinal column. Blanking itself is unchanged: **remove from
//! presence, emit no bytes**, never a sentinel.
//!
//! **No new retirement rule, and the two must not be conflated** (write-path §5.4). A suppression
//! touches no attribute artefact ever (Rule S) — there is no parameter here that could spell one —
//! and a deletion is executed only at the fold that performs it (Rule F), which is the `tombstones`
//! argument and nothing else. A coalesce therefore retires nothing: a deleted-but-unfolded entity's
//! key rides through untouched, exactly as its value does on the shipped axis.
//!
//! # Bounded by memory, not by time
//!
//! The merge holds one remap entry per input key — 4 B, unavoidable, since the remap is the pass's
//! output — and one decode buffer per layer. It holds **no** materialised key set: an arena of the
//! window's decoded keys would be several times the remap and would breach the input cap that
//! bounds the pass transient (§5.2), since decoding un-elides exactly the shared prefixes the
//! format exists to elide.
//!
//! The price is paid in decodes. [`tessera_filter::SortedDict`] offers a whole-file `walk` and a
//! random `key_of`, and `walk` is push-driven, so N of them cannot be interleaved into an N-way
//! merge; the cursor below therefore steps with `key_of`, which re-decodes its block from the
//! restart each time and so costs `(K + 1) / 2` — 8.5 entry decodes per key at the shipped restart
//! interval of 16. Against the dictionary campaign's **measured** 11.0–18.8 ns per key of decode
//! (`probes/2026-08-13-keyword-dict/`), that **models** to ~0.1–0.16 µs per key per pass: sub-second
//! for a coalesce window, seconds to tens of seconds for a fold at 10⁸ keys inside a nightly gated
//! window that already runs minutes to hours (decisions 0056, 0057). The alternative — spooling
//! each input's decoded keys to a temporary file and merging pull-style cursors over the spools —
//! buys back the constant at the cost of a second on-disk format and its own failure modes, and is
//! not taken while the pass is time-rich and memory-poor. Revisit with a measurement, not a hunch.
//!
//! # What is deliberately not here
//!
//! **Per-term keyword postings are not derived by either pass.** Where decision 0067 admits them
//! they serve whole-value operators only, and no producer emits them; deriving them here would put
//! an accelerator in the corpus ahead of the artefact it must be a derivative of.

use std::io::{self, BufWriter};
use std::path::Path;

use croaring::Bitmap;
use tessera_filter::{Access, Codes, ColumnKind, SortedDict, SortedDictWriter, ValueColumn};

use crate::{invalid, merge_order, write_merged, Runs};

/// A remap entry for an input key the rebuilt dictionary does not hold: its every carrier was
/// blanked. Only the fold can produce one, and an entity that still reached such a key would be a
/// contradiction — so [`write_merged`] refuses one rather than writing it.
pub(crate) const NO_KEY: u32 = u32::MAX;

/// One layer of a keyword column: the `u32` ordinal column, and the dictionary its ordinals name.
///
/// The pair is the unit because **an ordinal is a position in this layer's dictionary and means
/// nothing against another's**. Resolving one layer's ordinals against another's recolours the
/// layer with no symptom, which is why a layer's index files swap as one atomic manifest unit
/// (records §7). Taking the two together in the type is that rule made unspellable otherwise:
/// neither pass below can be handed a column without naming the dictionary that colours it.
#[derive(Clone, Copy)]
pub struct KeywordLayer<'a> {
    pub values: &'a ValueColumn,
    pub dict: &'a SortedDict,
}

/// One keyword column's extents merged into one, for the entity-space coalesce (index §5.2).
///
/// `inputs` is a window of one column's own extents, in any order — the merge sorts them by their
/// entity ranges and refuses an interleaving, exactly as every other axis does. What is written is
/// the same `(entity, key)` relation the inputs carried between them: three files, the merged
/// dictionary, the ordinals rewritten against it, and the presence bitmap an extent always carries.
///
/// **A coalesce retires nothing** — removal is the fold's (Rule F, write-path §5.4) — so there is
/// no tombstone parameter and no way to spell one.
pub fn coalesce_keyword_extents(
    inputs: &[KeywordLayer<'_>],
    values_path: &Path,
    presence_path: &Path,
    dict_path: &Path,
) -> io::Result<()> {
    const PASS: &str = "the keyword coalesce";
    if inputs.len() < 2 {
        return Err(invalid(format!(
            "{PASS} was given {} extents; it collapses a window of a column's extents into one and \
             there is nothing to collapse below two",
            inputs.len()
        )));
    }
    let columns = ordinal_columns(inputs, PASS)?;
    let (order, presence) = merge_order(&columns, &Bitmap::new(), PASS)?;
    // No liveness filter: every key in every input dictionary survives a coalesce, because every
    // entity does.
    let remap = merge_dictionaries(inputs, None, dict_path, PASS)?;
    verify_remap(inputs, &remap, dict_path, PASS)?;
    write_merged(
        &columns,
        &order,
        &Bitmap::new(),
        values_path,
        presence_path,
        Some(&presence),
        Some(&remap),
    )
}

/// One keyword column's layers merged into one base, with `tombstones` blanked and the dictionary
/// rebuilt from what survives (index §6.2).
///
/// `layers` is the base followed by every extent the fold consumes, in any order. `bound` is one
/// past the highest entity the fold's snapshot covers: the presence bitmap is **omitted** only when
/// every entity from 0 to that bound is present, which is the reader's dense-from-zero convention
/// and nothing looser. Returns whether a presence bitmap was written, so the caller can name
/// exactly the files that exist in the manifest.
///
/// `tombstones` is `D₀` — the deleted set this fold executes, Rule F's and only Rule F's.
pub fn fold_keyword_column(
    layers: &[KeywordLayer<'_>],
    tombstones: &Bitmap,
    bound: u32,
    values_path: &Path,
    presence_path: &Path,
    dict_path: &Path,
) -> io::Result<bool> {
    const PASS: &str = "the keyword fold";
    let columns = ordinal_columns(layers, PASS)?;
    let (order, out_presence) = merge_order(&columns, tombstones, PASS)?;
    // Dense from zero to the bound, and nothing looser — the same reconciliation
    // [`crate::fold_value_column`] makes, for the same reader convention.
    let universal = out_presence.cardinality() == u64::from(bound)
        && (bound == 0 || out_presence.maximum() == Some(bound - 1));
    let live = live_ordinals(layers, tombstones, PASS)?;
    let remap = merge_dictionaries(layers, Some(&live), dict_path, PASS)?;
    verify_remap(layers, &remap, dict_path, PASS)?;
    write_merged(
        &columns,
        &order,
        tombstones,
        values_path,
        presence_path,
        (!universal).then_some(&out_presence),
        Some(&remap),
    )?;
    Ok(!universal)
}

/// The layers' value columns, with the family checked once at the entry rather than discovered at
/// the first push.
///
/// A keyword layer stores `u32` ordinals. Any other width reaching here would be a column paired
/// with a dictionary it was never coloured by, which is the defect [`KeywordLayer`] exists to make
/// hard — so it refuses rather than being remapped as though its values were ordinals.
fn ordinal_columns<'a>(
    layers: &[KeywordLayer<'a>],
    pass: &str,
) -> io::Result<Vec<&'a ValueColumn>> {
    for layer in layers {
        if !matches!(layer.values.codes(), Codes::U32(_)) {
            return Err(invalid(format!(
                "{pass} was given a {:?} column; a keyword layer stores u32 ordinals into its own \
                 dictionary",
                ColumnKind::of(layer.values.codes())
            )));
        }
    }
    Ok(layers.iter().map(|layer| layer.values).collect())
}

/// Per layer, the ordinals a **surviving** entity still reaches.
///
/// This is the whole of what makes a key leave the dictionary: an ordinal absent from every layer's
/// live set is a key whose only carriers were blanked, so the merge never pushes it and its bytes
/// are not in the folded artefact. The pass costs one scan of each layer's ordinals, which is the
/// same count-then-write discipline the postings emit already takes.
fn live_ordinals(
    layers: &[KeywordLayer<'_>],
    tombstones: &Bitmap,
    pass: &str,
) -> io::Result<Vec<Bitmap>> {
    let mut live = Vec::with_capacity(layers.len());
    for layer in layers {
        let present = layer.values.present();
        let keep = present.andnot(tombstones);
        let Codes::U32(src) = layer.values.codes() else {
            return Err(invalid(format!(
                "{pass}: a keyword layer stores u32 ordinals"
            )));
        };
        let mut ordinals = Bitmap::new();
        let mut runs = Runs::new(&keep);
        while let Some((start, last)) = runs.next() {
            // A run of kept entities is contiguous in slot space as well as in entity space, which
            // is what lets the rank be taken once per run.
            let slot0 = (present.rank(start) - 1) as usize;
            for k in 0..=(last - start) as usize {
                let ordinal = *src.get(slot0 + k).ok_or_else(|| {
                    invalid(format!(
                        "{pass}: entity {} addresses slot {} in a {}-value layer",
                        start + k as u32,
                        slot0 + k,
                        src.len()
                    ))
                })?;
                ordinals.add(ordinal);
            }
        }
        live.push(ordinals);
    }
    Ok(live)
}

/// Merge the layers' sorted dictionaries into one at `dict_path`, returning `old -> new` per layer.
///
/// `live` restricts each layer to the ordinals a surviving entity reaches; `None` — the coalesce's
/// case — takes every key. Keys equal across layers collapse to one merged ordinal, which is the
/// interning a merge is for.
///
/// **The output is written at the default restart interval whatever the inputs used**, for the
/// reason the presence bitmap is normalised at the value writer (index §6.2): a rebuilt artefact
/// must be a function of its content alone, or a folded column and a freshly built one over the
/// same live entities would differ in bytes without differing in meaning.
///
/// The least key is found by a linear sweep of the layers rather than by a heap. The layer count is
/// the coalesce's width or the fold's surviving handful — tens, which §5.2 holds it at — where a
/// heap's log factor buys less than the branch it costs and the reader loses a loop they can check.
pub(crate) fn merge_dictionaries(
    layers: &[KeywordLayer<'_>],
    live: Option<&[Bitmap]>,
    dict_path: &Path,
    pass: &str,
) -> io::Result<Vec<Vec<u32>>> {
    let mut remap: Vec<Vec<u32>> = layers
        .iter()
        .map(|layer| vec![NO_KEY; layer.dict.len() as usize])
        .collect();
    let mut cursors = Vec::with_capacity(layers.len());
    for (i, layer) in layers.iter().enumerate() {
        cursors.push(KeyCursor::open(layer.dict, live.map(|sets| &sets[i]))?);
    }

    let mut writer = SortedDictWriter::new(BufWriter::new(std::fs::File::create(dict_path)?))?;
    let mut least: Vec<u8> = Vec::new();
    loop {
        let mut any = false;
        for cursor in &cursors {
            if let Some(key) = cursor.key() {
                if !any || key < least.as_slice() {
                    least.clear();
                    least.extend_from_slice(key);
                    any = true;
                }
            }
        }
        if !any {
            break;
        }
        // Valid UTF-8 by the reader's own decode check; re-derived here because `push` takes a
        // `&str` and a lossy conversion would put a replacement character in a key.
        let key = std::str::from_utf8(&least)
            .map_err(|e| invalid(format!("{pass}: a decoded key is not valid UTF-8: {e}")))?;
        // The writer refuses a key that does not ascend strictly, so a merge that lost its order
        // stops here rather than producing a dictionary whose ordinals mean nothing.
        let ordinal = writer.push(key)?;
        for (i, cursor) in cursors.iter_mut().enumerate() {
            if cursor.key() == Some(least.as_slice()) {
                remap[i][cursor.at() as usize] = ordinal;
                cursor.advance()?;
            }
        }
    }
    writer.finish()?;
    Ok(remap)
}

/// The merge's content guard, discharged against the dictionary **as written and read back**,
/// before any ordinal is written.
///
/// Reading the file back rather than checking the merge's own intent is the point: what must hold
/// is a property of the artefact about to be published, and the merge is the thing under suspicion.
/// It also runs the reader's total per-entry checks over the whole merged file, so a dictionary
/// that decodes wrongly fails here as well.
///
/// Three properties, each catching a defect the others do not:
///
/// - **Monotone and in range.** Both dictionaries are sorted, so a correct remap ascends strictly;
///   a swap or a collapse fails here.
/// - **`merged[remap[i]] == input[i]` for every input key.** A remap can be monotone, in range and
///   wrong — shifted onto a neighbour's key — and nothing else in either pass would notice.
/// - **Every merged key is claimed by some input.** For a coalesce this refuses a key no input
///   carries; for a fold it is the retention statement, since a key whose carriers were all blanked
///   must not be in the rebuilt dictionary at all.
///
/// The cost is one sequential walk of the merged dictionary and one `key_of` per live input key —
/// O(keys), on a pass already streaming both.
pub(crate) fn verify_remap(
    layers: &[KeywordLayer<'_>],
    remap: &[Vec<u32>],
    dict_path: &Path,
    pass: &str,
) -> io::Result<()> {
    // The pass owns this mapping — it wrote the file — so the sequential hint is its to give
    // (decision 0052): the guard reads the whole file once, end to end.
    let merged = SortedDict::open(dict_path, Access::MappedSequential)?;

    for (i, table) in remap.iter().enumerate() {
        let mut previous: Option<u32> = None;
        for (old, &new) in table.iter().enumerate() {
            if new == NO_KEY {
                continue;
            }
            if new >= merged.len() {
                return Err(invalid(format!(
                    "{pass}: layer {i}'s ordinal {old} remaps to {new}, past the merged \
                     dictionary's {} keys",
                    merged.len()
                )));
            }
            if let Some(previous) = previous {
                if new <= previous {
                    return Err(invalid(format!(
                        "{pass}: layer {i}'s remap is not monotone at ordinal {old} — {new} does \
                         not follow {previous}, and both dictionaries are sorted, so a correct \
                         remap ascends strictly"
                    )));
                }
            }
            previous = Some(new);
        }
    }

    let mut at = vec![0usize; layers.len()];
    let mut scratch: Vec<Vec<u8>> = vec![Vec::new(); layers.len()];
    let mut failure: Option<String> = None;
    merged.walk(|new, key| {
        if failure.is_some() {
            return;
        }
        let mut claimed = false;
        for (i, table) in remap.iter().enumerate() {
            while at[i] < table.len() && table[at[i]] == NO_KEY {
                at[i] += 1;
            }
            // Strict monotonicity, checked above, makes the remap injective: at most one of a
            // layer's ordinals reaches this merged one, and the walk is ascending, so a layer whose
            // cursor has not reached `new` has nothing here.
            if at[i] < table.len() && table[at[i]] == new {
                let old = at[i] as u32;
                match layers[i].dict.key_of(old, &mut scratch[i]) {
                    Ok(theirs) if theirs == key => {}
                    Ok(theirs) => {
                        failure = Some(format!(
                            "{pass}: layer {i}'s ordinal {old} carries {theirs:?} but remaps to \
                             merged ordinal {new}, which carries {key:?} — the remap would \
                             recolour every entity holding that key"
                        ));
                    }
                    Err(e) => failure = Some(format!("{pass}: layer {i}'s ordinal {old}: {e}")),
                }
                at[i] += 1;
                claimed = true;
            }
        }
        if !claimed && failure.is_none() {
            failure = Some(format!(
                "{pass}: the merged dictionary holds {key:?} at ordinal {new}, which no input key \
                 remaps to — a key nothing carries"
            ));
        }
    })?;
    if let Some(detail) = failure {
        return Err(invalid(detail));
    }

    for (i, table) in remap.iter().enumerate() {
        while at[i] < table.len() && table[at[i]] == NO_KEY {
            at[i] += 1;
        }
        if at[i] != table.len() {
            return Err(invalid(format!(
                "{pass}: layer {i}'s ordinal {} remaps to {}, which the merged dictionary's walk \
                 never reached",
                at[i], table[at[i]]
            )));
        }
    }
    Ok(())
}

/// A sequential reader over one dictionary's live keys, stepping with
/// [`tessera_filter::SortedDict::key_of`].
///
/// The module doc argues the constant this costs and the spool that would buy it back. What the
/// cursor gives in exchange is that N of them coexist, which `walk` — push-driven and without an
/// early exit, both deliberate — cannot offer.
struct KeyCursor<'a> {
    dict: &'a SortedDict,
    live: Option<&'a Bitmap>,
    /// The ordinal whose key `scratch` holds; `dict.len()` once exhausted.
    at: u32,
    scratch: Vec<u8>,
}

impl<'a> KeyCursor<'a> {
    fn open(dict: &'a SortedDict, live: Option<&'a Bitmap>) -> io::Result<Self> {
        let mut cursor = KeyCursor {
            dict,
            live,
            at: 0,
            scratch: Vec::new(),
        };
        cursor.seek()?;
        Ok(cursor)
    }

    /// The key this cursor is positioned on, or `None` once it is exhausted.
    fn key(&self) -> Option<&[u8]> {
        (self.at < self.dict.len()).then_some(self.scratch.as_slice())
    }

    fn at(&self) -> u32 {
        self.at
    }

    fn advance(&mut self) -> io::Result<()> {
        self.at += 1;
        self.seek()
    }

    /// Settle on the first live ordinal at or after `at`, decoding its key.
    fn seek(&mut self) -> io::Result<()> {
        while self.at < self.dict.len() {
            if self.live.is_none_or(|live| live.contains(self.at)) {
                self.dict.key_of(self.at, &mut self.scratch)?;
                return Ok(());
            }
            self.at += 1;
        }
        self.scratch.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::buffer::ScalarBuffer;
    use tessera_filter::{write_sorted_dict, write_value_column, SortedDict};

    fn bitmap(entities: impl IntoIterator<Item = u32>) -> Bitmap {
        let mut b = Bitmap::new();
        for e in entities {
            b.add(e);
        }
        b
    }

    /// A keyword layer over `(entity, key)` pairs: the layer's own sorted dictionary, and one
    /// ordinal into it per present entity.
    ///
    /// **Built here because nothing else builds one yet.** The base build and the flush that will
    /// produce these layers are a parallel track's; the two libraries this needs — the dictionary
    /// writer and the value-column writer — are the ones the passes under test consume, so an
    /// in-process layer is the real artefact rather than a stand-in for it.
    struct Layer {
        column: ValueColumn,
        dict: SortedDict,
    }

    impl Layer {
        fn as_ref(&self) -> KeywordLayer<'_> {
            KeywordLayer {
                values: &self.column,
                dict: &self.dict,
            }
        }
    }

    fn layer(dir: &Path, tag: &str, pairs: &[(u32, &str)]) -> Layer {
        let mut keys: Vec<&str> = pairs.iter().map(|(_, k)| *k).collect();
        keys.sort_unstable();
        keys.dedup();
        let path = dir.join(format!("{tag}-dict.bin"));
        write_sorted_dict(&path, keys.iter().copied()).expect("the dictionary writes");
        let dict = SortedDict::open(&path, Access::Read).expect("it opens");

        let mut entities: Vec<(u32, &str)> = pairs.to_vec();
        entities.sort_unstable();
        let ordinals: Vec<u32> = entities
            .iter()
            .map(|(_, key)| keys.binary_search(key).expect("a key of this layer") as u32)
            .collect();
        let present = bitmap(entities.iter().map(|(e, _)| *e));
        let column = ValueColumn::partial(Codes::U32(ScalarBuffer::from(ordinals)), present)
            .expect("an extent");
        Layer { column, dict }
    }

    /// The key `entity` reads back through a column and the dictionary that colours it.
    fn key_of(column: &ValueColumn, dict: &SortedDict, entity: u32) -> Option<String> {
        let ordinal = column.value_of(entity)?.raw();
        let mut scratch = Vec::new();
        Some(
            dict.key_of(ordinal, &mut scratch)
                .expect("a key")
                .to_string(),
        )
    }

    /// Open the three files a keyword pass writes. The presence file is passed only where one
    /// exists: a folded base dense from zero writes none, and its absence is the addressing.
    fn open_output(dir: &Path, tag: &str) -> (ValueColumn, SortedDict) {
        let (values_path, presence_path, dict_path) = paths(dir, tag);
        let column = ValueColumn::open(
            &values_path,
            presence_path.exists().then_some(presence_path.as_path()),
            Access::Read,
        )
        .expect("the column opens");
        let dict = SortedDict::open(&dict_path, Access::Read).expect("the dictionary opens");
        (column, dict)
    }

    fn paths(
        dir: &Path,
        tag: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        (
            dir.join(format!("{tag}-values.arrow")),
            dir.join(format!("{tag}-presence.roaring")),
            dir.join(format!("{tag}-dict.bin")),
        )
    }

    fn coalesce(dir: &Path, tag: &str, inputs: &[KeywordLayer<'_>]) -> io::Result<()> {
        let (values, presence, dict) = paths(dir, tag);
        coalesce_keyword_extents(inputs, &values, &presence, &dict)
    }

    /// **A coalesced extent carries exactly the `(entity, key)` relation its inputs carried between
    /// them**, through a dictionary merge that renumbers every ordinal — which is the whole of the
    /// content-preserving claim for this family. Asserted over key sets that overlap (so the merge
    /// interns), over a key exclusive to one input, and over a *second* coalesce of the first's
    /// output, which is the recursion the per-column selection unit makes free.
    #[test]
    fn a_coalesced_keyword_extent_reads_back_every_entitys_own_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Disjoint, ascending entity ranges with gaps — the shape a flush publishes — and key sets
        // that share some values and not others, so the merge both interns and extends.
        let first = layer(
            dir.path(),
            "a",
            &[(100, "delta"), (101, "alpha"), (104, "delta")],
        );
        let second = layer(
            dir.path(),
            "b",
            &[(200, "bravo"), (203, "alpha"), (204, "echo")],
        );
        let third = layer(dir.path(), "c", &[(300, "charlie"), (301, "bravo")]);
        let inputs = [first.as_ref(), second.as_ref(), third.as_ref()];

        coalesce(dir.path(), "merged", &inputs).expect("the coalesce");
        let (column, dict) = open_output(dir.path(), "merged");

        let expected = [
            (100, "delta"),
            (101, "alpha"),
            (104, "delta"),
            (200, "bravo"),
            (203, "alpha"),
            (204, "echo"),
            (300, "charlie"),
            (301, "bravo"),
        ];
        for (entity, key) in expected {
            assert_eq!(
                key_of(&column, &dict, entity).as_deref(),
                Some(key),
                "entity {entity} reads back another entity's key"
            );
        }
        // The merged dictionary is the sorted union of the inputs', interned: five keys, not eight.
        let mut merged_keys = Vec::new();
        dict.walk(|_, key| merged_keys.push(key.to_string()))
            .expect("the merged dictionary walks");
        assert_eq!(merged_keys, ["alpha", "bravo", "charlie", "delta", "echo"]);
        assert_eq!(
            column.present().iter().collect::<Vec<_>>(),
            expected.iter().map(|(e, _)| *e).collect::<Vec<_>>()
        );

        // The recursion: a coalesced extent is an extent like any other, and the next rung takes it
        // identically — including its already-merged dictionary.
        let fourth = layer(dir.path(), "d", &[(400, "foxtrot"), (401, "alpha")]);
        let again = [
            KeywordLayer {
                values: &column,
                dict: &dict,
            },
            fourth.as_ref(),
        ];
        coalesce(dir.path(), "again", &again).expect("the recursion");
        let (column, dict) = open_output(dir.path(), "again");
        for (entity, key) in expected
            .iter()
            .copied()
            .chain([(400, "foxtrot"), (401, "alpha")])
        {
            assert_eq!(key_of(&column, &dict, entity).as_deref(), Some(key));
        }
    }

    /// **The guard fires on a remap that is monotone, in range and wrong — and the inherited guards
    /// are silent on the same defect.** This is the whole reason this merge carries a guard of its
    /// own: recolouring every value changes no cardinality, so union-equals-sum over presence and
    /// the non-interleaving check pass over a window whose every key has moved.
    ///
    /// **Fault injected**, three ways, all executed here rather than described: a remap shifted onto
    /// a neighbour's key, one whose entries swap, and one reaching past the merged dictionary. The
    /// third part of the test then *skips* the guard and shows what publishing would have meant.
    #[test]
    fn a_monotone_but_wrong_remap_is_refused_where_the_shipped_guards_are_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Interleaved key sets, so a shift by one lands a whole layer on the other layer's keys.
        let first = layer(dir.path(), "a", &[(10, "alpha"), (11, "charlie")]);
        let second = layer(dir.path(), "b", &[(20, "bravo"), (21, "delta")]);
        let inputs = [first.as_ref(), second.as_ref()];
        let (values_path, presence_path, dict_path) = paths(dir.path(), "guard");

        let columns = ordinal_columns(&inputs, "the test").expect("u32 layers");
        let (order, presence) =
            merge_order(&columns, &Bitmap::new(), "the test").expect("the shipped guards pass");
        let remap = merge_dictionaries(&inputs, None, &dict_path, "the test").expect("the merge");
        assert_eq!(
            remap,
            vec![vec![0, 2], vec![1, 3]],
            "alpha bravo charlie delta"
        );
        verify_remap(&inputs, &remap, &dict_path, "the test").expect("a correct remap passes");

        // Monotone, in range, and wrong: the two layers' tables cross, so every merged ordinal is
        // still claimed exactly once and every entity lands on the other layer's key. Nothing but
        // the content comparison can see this.
        let crossed = vec![remap[1].clone(), remap[0].clone()];
        let err = verify_remap(&inputs, &crossed, &dict_path, "the test")
            .expect_err("a crossed remap is refused");
        assert!(err.to_string().contains("recolour"), "{err}");
        assert!(
            !values_path.exists(),
            "the guard must refuse before any ordinal is written"
        );

        // Not monotone: two of a layer's ordinals cross.
        let swapped = vec![vec![2, 0], remap[1].clone()];
        let err = verify_remap(&inputs, &swapped, &dict_path, "the test")
            .expect_err("a non-monotone remap is refused");
        assert!(err.to_string().contains("not monotone"), "{err}");

        // Past the end of the merged dictionary.
        let past = vec![vec![0, 9], remap[1].clone()];
        let err = verify_remap(&inputs, &past, &dict_path, "the test")
            .expect_err("an out-of-range remap is refused");
        assert!(err.to_string().contains("past the merged"), "{err}");

        // And what the guard is standing in front of: with it skipped, the crossed remap publishes
        // a clean-looking extent whose every entity reads another entity's key, while the presence
        // the inherited guards test is bit-for-bit what a correct merge would have written.
        write_merged(
            &columns,
            &order,
            &Bitmap::new(),
            &values_path,
            &presence_path,
            Some(&presence),
            Some(&crossed),
        )
        .expect("the recoloured merge writes happily");
        let (column, dict) = open_output(dir.path(), "guard");
        assert_eq!(key_of(&column, &dict, 10).as_deref(), Some("bravo"));
        assert_eq!(key_of(&column, &dict, 11).as_deref(), Some("delta"));
        assert_eq!(
            column.present(),
            presence,
            "presence is untouched by recolouring, which is why the inherited guards cannot see it"
        );
    }

    /// **A merged dictionary holding a key no input carries is refused.** The same check read the
    /// other way is the fold's retention statement, so it is asserted directly rather than only
    /// through the fold that depends on it.
    #[test]
    fn a_merged_key_no_input_remaps_to_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = layer(dir.path(), "a", &[(10, "alpha")]);
        let second = layer(dir.path(), "b", &[(20, "charlie")]);
        let inputs = [first.as_ref(), second.as_ref()];
        let dict_path = dir.path().join("spurious-dict.bin");
        write_sorted_dict(&dict_path, ["alpha", "bravo", "charlie"]).expect("a dictionary");
        let remap = vec![vec![0], vec![2]];
        let err = verify_remap(&inputs, &remap, &dict_path, "the test")
            .expect_err("a key nothing carries is refused");
        assert!(err.to_string().contains("nothing carries"), "{err}");
    }

    /// Fold `layers` and return the three files' bytes — the presence file's `None` where the
    /// folded column is dense from zero and its absence carries the meaning.
    fn fold_to_bytes(
        dir: &Path,
        tag: &str,
        layers: &[KeywordLayer<'_>],
        tombstones: &Bitmap,
        bound: u32,
    ) -> (Vec<u8>, Option<Vec<u8>>, Vec<u8>) {
        let (values, presence, dict) = paths(dir, tag);
        let partial = fold_keyword_column(layers, tombstones, bound, &values, &presence, &dict)
            .expect("the fold");
        (
            std::fs::read(&values).expect("values"),
            partial.then(|| std::fs::read(&presence).expect("presence")),
            std::fs::read(&dict).expect("dict"),
        )
    }

    /// A single build of a keyword column over `pairs` — dictionary, ordinals and presence — which
    /// is what the fold's three files are measured against. `universal` is the build's own
    /// dense-from-zero judgement, made by the caller exactly as the fold makes it from its bound.
    fn build_one(
        dir: &Path,
        tag: &str,
        pairs: &[(u32, &str)],
        universal: bool,
    ) -> (Vec<u8>, Option<Vec<u8>>, Vec<u8>) {
        let built = layer(dir, tag, pairs);
        let (values, presence, dict) = paths(dir, tag);
        let present = built.column.present();
        write_value_column(
            &values,
            &presence,
            built.column.codes(),
            (!universal).then_some(&present),
        )
        .expect("the one-shot writer");
        (
            std::fs::read(&values).expect("values"),
            (!universal).then(|| std::fs::read(&presence).expect("presence")),
            std::fs::read(&dict).expect("dict"),
        )
    }

    /// **A folded keyword column is byte-identical to what a single build over the same live
    /// entities would have written** — all three files, the rebuilt dictionary included.
    ///
    /// index §6.2's equivalence claim is byte-identity rather than a tolerance, and for this family
    /// it reaches the dictionary: the rebuild normalises the restart interval and the merge emits
    /// keys in the one canonical encoding the format admits, so equal content is equal bytes.
    /// Asserted over the universal shape — where the presence file's *absence* is the meaning — and
    /// over a blanked one.
    ///
    /// **Fault injected:** pass `None` for the fold's liveness filter — the rebuild then keeps the
    /// blanked entity's key — and the second half fails on the dictionary's bytes. Verified
    /// non-vacuous that way.
    #[test]
    fn a_folded_keyword_column_is_the_bytes_a_single_build_would_have_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = [
            (0u32, "delta"),
            (1, "alpha"),
            (2, "delta"),
            (3, "bravo"),
            (4, "alpha"),
            (5, "echo"),
        ];
        let base = layer(dir.path(), "base", &live[..4]);
        let first = layer(dir.path(), "ext1", &live[4..5]);
        let second = layer(dir.path(), "ext2", &live[5..]);
        // Listed out of order: the merge sorts the layers by their own entity ranges.
        let layers = [second.as_ref(), base.as_ref(), first.as_ref()];

        let (values, presence, dict) =
            fold_to_bytes(dir.path(), "universal", &layers, &Bitmap::new(), 6);
        assert!(
            presence.is_none(),
            "dense from zero to the bound, so the presence file's absence carries the meaning"
        );
        let built = build_one(dir.path(), "whole", &live, true);
        assert_eq!(values, built.0, "the folded values are not a build's");
        assert_eq!(dict, built.2, "the rebuilt dictionary is not a build's");

        // And with an entity blanked: the same three files a build over the survivors alone writes.
        let tombstones = bitmap([3]);
        let (values, presence, dict) =
            fold_to_bytes(dir.path(), "blanked", &layers, &tombstones, 6);
        let survivors: Vec<(u32, &str)> = live.iter().copied().filter(|(e, _)| *e != 3).collect();
        let built = build_one(dir.path(), "survivors", &survivors, false);
        assert_eq!(values, built.0);
        assert_eq!(
            presence.expect("a blanked column is partial"),
            built.1.expect("presence")
        );
        assert_eq!(dict, built.2);
    }

    /// **A key whose only carrier was blanked leaves the dictionary entirely** — the retention
    /// argument reaching dictionary keys, asserted against the file's own bytes rather than through
    /// a reader that could be answering from a stale ordinal.
    ///
    /// The shared key is the control: `bravo` also has a surviving carrier, so it stays, and a fold
    /// that dropped keys by carrier count rather than by survivor would fail on it.
    ///
    /// **Fault injected:** pass `None` for the fold's liveness filter and the rebuild keeps every
    /// key its inputs held, blanked carriers included — this fails on the file's own bytes.
    #[test]
    fn a_blanked_entitys_only_key_leaves_the_folded_dictionary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = layer(
            dir.path(),
            "base",
            &[(0, "alpha"), (1, "solitary"), (2, "bravo"), (3, "bravo")],
        );
        let layers = [base.as_ref()];
        // Entities 1 and 3 are deleted: `solitary` loses its only carrier, `bravo` keeps one.
        let (values, _, dict) = fold_to_bytes(dir.path(), "retained", &layers, &bitmap([1, 3]), 4);
        assert!(
            !dict.windows(8).any(|w| w == b"solitary"),
            "the blanked entity's key is still in the rebuilt dictionary"
        );
        assert!(dict.windows(5).any(|w| w == b"bravo"));
        assert!(!values.is_empty());

        let (column, dict) = open_output(dir.path(), "retained");
        let mut keys = Vec::new();
        dict.walk(|_, key| keys.push(key.to_string()))
            .expect("the rebuilt dictionary walks");
        assert_eq!(
            keys,
            ["alpha", "bravo"],
            "the dictionary is the surviving keys"
        );
        assert_eq!(key_of(&column, &dict, 0).as_deref(), Some("alpha"));
        assert_eq!(key_of(&column, &dict, 2).as_deref(), Some("bravo"));
        assert_eq!(
            column.value_of(1),
            None,
            "blanking is removal from presence, never a sentinel value"
        );
    }

    /// **Rule S and Rule F, which must never be conflated** (write-path §5.4).
    ///
    /// Rule F: a deletion is executed at the fold and nowhere else, so a coalesce carries a
    /// deleted-but-unfolded entity's key through untouched — there is no tombstone parameter on the
    /// coalesce and no way to spell one. Rule S: a suppression retires only by its unsuppress and
    /// touches no attribute artefact ever, so it is not in the fold's blanked set either and its
    /// entity's key survives the rebuild.
    ///
    /// **Fault injected:** give `coalesce_keyword_extents` a tombstone set to pass through and the
    /// first half fails on the missing entity; add a suppressed entity to the fold's `tombstones`
    /// and the second half fails on the missing key.
    #[test]
    fn a_coalesce_retires_nothing_and_a_suppression_reaches_no_keyword_artefact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = layer(dir.path(), "a", &[(100, "kept"), (101, "deleted-key")]);
        let second = layer(dir.path(), "b", &[(200, "suppressed-key")]);
        let inputs = [first.as_ref(), second.as_ref()];
        coalesce(dir.path(), "rides", &inputs).expect("the coalesce");
        let (column, dict) = open_output(dir.path(), "rides");
        // Entity 101 is deleted-but-unfolded: it is in the overlay's `deleted` set, and no
        // attribute artefact knows that or may act on it.
        assert_eq!(key_of(&column, &dict, 101).as_deref(), Some("deleted-key"));

        // Entity 200 is suppressed. The fold's blanked set is `D₀` — deletions — and a suppression
        // is never in it, so the rebuild keeps the key its only carrier holds.
        let coalesced = KeywordLayer {
            values: &column,
            dict: &dict,
        };
        let (_, _, dict_bytes) =
            fold_to_bytes(dir.path(), "folded", &[coalesced], &bitmap([101]), 201);
        let (column, dict) = open_output(dir.path(), "folded");
        assert_eq!(
            key_of(&column, &dict, 200).as_deref(),
            Some("suppressed-key")
        );
        assert_eq!(key_of(&column, &dict, 100).as_deref(), Some("kept"));
        assert_eq!(column.value_of(101), None, "the fold executed the deletion");
        assert!(
            !dict_bytes.windows(11).any(|w| w == b"deleted-key"),
            "the deleted entity's only key is still in the folded dictionary"
        );
    }

    /// **The inherited guards still fire through the keyword entry points**, because they guard the
    /// entity relation and this pass changes only the values. Sharing the merge is what makes that
    /// true rather than a second copy that agrees.
    #[test]
    fn overlapping_and_interleaved_keyword_layers_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = layer(dir.path(), "a", &[(7, "alpha"), (8, "bravo")]);
        let clash = layer(dir.path(), "b", &[(8, "charlie")]);
        let err = coalesce(dir.path(), "clash", &[first.as_ref(), clash.as_ref()])
            .expect_err("an overlap is refused");
        assert!(err.to_string().contains("twice"), "{err}");

        let odd = layer(dir.path(), "odd", &[(1, "alpha"), (3, "charlie")]);
        let even = layer(dir.path(), "even", &[(0, "bravo"), (2, "delta")]);
        let err = coalesce(dir.path(), "interleaved", &[odd.as_ref(), even.as_ref()])
            .expect_err("interleaving is refused");
        assert!(err.to_string().contains("interleaved"), "{err}");

        // And a single extent is not a window: the pass collapses several into one.
        assert!(coalesce(dir.path(), "alone", &[first.as_ref()]).is_err());
    }

    /// **An ordinal outside its own layer's dictionary refuses rather than being remapped.** It is
    /// the symptom of a column paired with another layer's dictionary — the recolouring records §7
    /// makes a layer's files one atomic manifest unit to prevent — and it has no other symptom.
    #[test]
    fn an_ordinal_outside_its_layers_dictionary_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = layer(dir.path(), "a", &[(10, "alpha")]);
        let second = layer(dir.path(), "b", &[(20, "bravo")]);
        // A column claiming an ordinal its two-key dictionary does not hold.
        let stray = ValueColumn::partial(Codes::U32(ScalarBuffer::from(vec![9u32])), bitmap([30]))
            .expect("a column");
        let inputs = [
            first.as_ref(),
            second.as_ref(),
            KeywordLayer {
                values: &stray,
                dict: &first.dict,
            },
        ];
        let err = coalesce(dir.path(), "stray", &inputs).expect_err("a stray ordinal is refused");
        assert!(err.to_string().contains("dictionary"), "{err}");
    }

    /// **A layer whose values are not ordinals is refused at the entry**, rather than at the first
    /// push where the message would name a width mismatch instead of the pairing that caused it.
    #[test]
    fn a_column_that_is_not_an_ordinal_column_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = layer(dir.path(), "a", &[(10, "alpha")]);
        let text = ValueColumn::partial(Codes::text(["alpha".to_string()]), bitmap([20]))
            .expect("a text column");
        let inputs = [
            first.as_ref(),
            KeywordLayer {
                values: &text,
                dict: &first.dict,
            },
        ];
        let err = coalesce(dir.path(), "text", &inputs).expect_err("a text layer is refused");
        assert!(err.to_string().contains("u32 ordinals"), "{err}");
    }
}
