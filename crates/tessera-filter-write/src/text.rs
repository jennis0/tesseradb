//! The text family's two lifecycle transformations: the fold's merge of every layer into one base,
//! and the coalesce's merge of a window of extents into one extent (`records-and-search.md` §7;
//! `filter-index.md` §5.2 for the coalesce, §6.2 for the fold).
//!
//! **One merge, two obligations**, exactly as the value and keyword families are arranged here. The
//! k-way walk below is the whole of both passes: every term of every layer in sorted order, its
//! posting the union of the layers that hold it, written into one dictionary and one postings file.
//! What differs is small, and getting either wrong is silent:
//!
//! - The fold **retires**: `D₀`'s entities are subtracted from every posting and a term whose every
//!   carrier was blanked is not written at all, so the word leaves the corpus with the documents
//!   that used it. A coalesce **retires nothing** — removal is Rule F's alone (write-path §5.4) —
//!   so it passes an empty tombstone set and there is no parameter that could spell anything else.
//! - The fold writes **no presence bitmap**, because its output is a *base* and the base build
//!   writes none. A coalesced extent always writes one: it is a layer, and a reader must be able to
//!   say which entities it stands for.
//!
//! # Why a text column may be coalesced where a keyword column may not
//!
//! Both families carry a **per-layer dictionary**, and a coalesce of either renumbers: the merged
//! dictionary is a new key set and every ordinal in the output means a position in it. For the
//! keyword family that is why the engine's coalesce declines the column — a coalesced `AttrExtent`
//! is composed as *values alone*, so the merged dictionary would have nowhere to be installed and
//! the extent's ordinals would resolve against dictionaries that no longer number them.
//!
//! A text layer has no such gap: its dictionary, its postings and its presence are **one manifest
//! record** (`TextExtent`), composed together and replaced together. So the renumbering never
//! escapes the layer, and no remap of anything outside it is owed — which is why this merge carries
//! no counterpart to the keyword module's [`crate::coalesce_keyword_extents`] remap guard. There is
//! nothing stored per entity to remap: a text column's index is postings over terms, and its values
//! are record-blob rows the pass never touches.
//!
//! # What the merge checks
//!
//! **Posting *i* is term *i*'s, and nothing else says so.** A layer whose postings file is short of
//! its dictionary answers "carried by nobody" for every ordinal past the gap, so merging it would
//! write that under-report into the output where no later pass could find it. Each input's two
//! halves are checked against each other before a term is read — the same check the reader makes
//! before serving a layer, and the writer's consumer refusing what the reader refuses.
//!
//! The dictionary writer refuses a key that does not ascend strictly, so a merge that lost its
//! order stops there rather than publishing a dictionary whose ordinals mean nothing.

use std::io;
use std::path::Path;

use croaring::Bitmap;
use tessera_authz::PostingsSpool;
use tessera_filter::{ColumnPostings, SortedDict, SortedDictWriter};
use tessera_types::SMALL_TERM_THRESHOLD_DEFAULT;

/// One input layer: its dictionary, its postings over that dictionary, and the entities it holds a
/// value for.
///
/// **The three travel together because an ordinal is a position in *this* dictionary.** `present`
/// is `None` for the build's base layer, which writes no presence file at all — a fold takes one
/// and a coalesce never does, which is why the field is an option here and required there.
pub struct TextLayerRef<'a> {
    pub dict: &'a SortedDict,
    pub postings: &'a ColumnPostings,
    pub present: Option<&'a Bitmap>,
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The fold's pass: every layer merged into one base index, with `tombstones` subtracted.
///
/// `spool_path` is the pass's own scratch — the postings are spooled and assembled, this repo's
/// discipline for a file whose records are sized as they are written. **The caller owns its
/// removal on every exit path** (compaction §8), which is why it is a parameter rather than a
/// temporary this function makes and unlinks: a fold that failed halfway must leave nothing behind,
/// and the one place that knows every exit path is the caller.
pub fn merge_text_layers(
    inputs: &[TextLayerRef<'_>],
    tombstones: &Bitmap,
    dict_path: &Path,
    postings_path: &Path,
    spool_path: &Path,
) -> io::Result<()> {
    merge(inputs, tombstones, dict_path, postings_path, spool_path)
}

/// The coalesce's pass: a window of a column's extents merged into one extent.
///
/// Retires nothing, and writes the presence union its replacement rule is checked against — the
/// engine's composition requires a coalesced layer's presence to **equal** the union of the layers
/// it replaces, or the column's coverage drifts and every later disjointness check tests against
/// the wrong set.
///
/// Refuses fewer than two inputs: this collapses a window into one extent and there is nothing to
/// collapse below two. Refuses an input carrying no presence, which is the build's base — a pass
/// that consumed the base would be rewriting a file the bundle manifest names, which is compaction
/// under another name (`crate::coalesce`'s module doc in the engine).
pub fn coalesce_text_extents(
    inputs: &[TextLayerRef<'_>],
    dict_path: &Path,
    postings_path: &Path,
    presence_path: &Path,
    spool_path: &Path,
) -> io::Result<()> {
    const PASS: &str = "the text coalesce";
    if inputs.len() < 2 {
        return Err(invalid(format!(
            "{PASS} was given {} extents; it collapses a window of a column's extents into one and \
             there is nothing to collapse below two",
            inputs.len()
        )));
    }
    let mut union = Bitmap::new();
    for (i, input) in inputs.iter().enumerate() {
        let Some(present) = input.present else {
            return Err(invalid(format!(
                "{PASS}: input layer {i} carries no presence bitmap, so it is the build's base \
                 index; a coalesce takes extents only, the base being named in MANIFEST.files and \
                 rewritable by a fold alone"
            )));
        };
        union |= present;
    }
    merge(inputs, &Bitmap::new(), dict_path, postings_path, spool_path)?;
    std::fs::write(presence_path, union.serialize::<croaring::Portable>())
}

/// The k-way merge itself: every term of every layer, in sorted order, with `tombstones` subtracted
/// from each one's postings and an emptied term dropped.
fn merge(
    inputs: &[TextLayerRef<'_>],
    tombstones: &Bitmap,
    dict_path: &Path,
    postings_path: &Path,
    spool_path: &Path,
) -> io::Result<()> {
    for (i, input) in inputs.iter().enumerate() {
        if input.dict.len() != input.postings.record_count() {
            return Err(invalid(format!(
                "text layer {i} holds {} terms but {} postings records; an ordinal names a \
                 position in its own layer's dictionary, so merging these would attribute terms to \
                 the wrong words",
                input.dict.len(),
                input.postings.record_count()
            )));
        }
    }

    let mut writer = SortedDictWriter::new(std::io::BufWriter::new(std::fs::File::create(
        dict_path,
    )?))?;
    let mut spool = PostingsSpool::create(spool_path)?;

    // One decode buffer for the whole merge, and one owned key per layer: `key_of` borrows the
    // scratch it decodes into, and a cursor has to hold its key across the comparisons that pick
    // the least. `None` is an exhausted layer.
    let mut scratch: Vec<u8> = Vec::new();
    let mut at: Vec<u32> = vec![0; inputs.len()];
    let mut current: Vec<Option<String>> = Vec::with_capacity(inputs.len());
    for input in inputs {
        current.push(cursor_key(input.dict, 0, &mut scratch)?);
    }

    let mut least = String::new();
    loop {
        // Linear over the layers rather than through a heap: the layer count is one base plus the
        // flush extents published since the last fold, so it is small, and a heap would cost a
        // clone per term to save a comparison per term.
        let mut chosen: Option<usize> = None;
        for (i, key) in current.iter().enumerate() {
            let Some(key) = key else { continue };
            if chosen.is_none_or(|c| key < current[c].as_ref().expect("a chosen layer has a key")) {
                chosen = Some(i);
            }
        }
        let Some(chosen) = chosen else { break };
        least.clear();
        least.push_str(
            current[chosen]
                .as_ref()
                .expect("the chosen layer has a key"),
        );

        // **Every layer holding this term, not the first**: a word in two layers is one term of the
        // merged index, and its posting is the union of theirs.
        let mut entities = Bitmap::new();
        for i in 0..inputs.len() {
            if current[i].as_deref() != Some(least.as_str()) {
                continue;
            }
            entities |= inputs[i]
                .postings
                .entities(tessera_types::AttrLocalId::new(at[i]))?;
            at[i] += 1;
            current[i] = cursor_key(inputs[i].dict, at[i], &mut scratch)?;
        }
        entities.andnot_inplace(tombstones);
        // **The retention statement.** A term every one of whose carriers was blanked is not
        // written — not as an empty posting, not as a dictionary key — so the word leaves the
        // corpus with the documents that used it. With an empty tombstone set, as a coalesce
        // passes, nothing here can fire: no input posting is empty, the writers never emit one.
        if entities.is_empty() {
            continue;
        }
        // The writer refuses a key that does not ascend strictly, so a merge that lost its order
        // stops here rather than publishing a dictionary whose ordinals mean nothing.
        writer.push(&least)?;
        // **The default threshold, not the bundle's `small_term_threshold`.** That field is the
        // term dictionary's, chosen for authorisation postings; the writers that produce a text
        // index — the build, the flush, the fold and the coalesce — all take the crate default, and
        // one that took the other would re-tag every posting on the boundary and stop reproducing
        // the build it is supposed to agree with.
        let record =
            tessera_authz::postings::encode_posting_bitmap(&entities, SMALL_TERM_THRESHOLD_DEFAULT)?;
        spool.append(&record)?;
    }

    writer.finish()?;
    spool.finish(postings_path)?;
    Ok(())
}

/// The key at `ordinal`, owned, or `None` where the ordinal is past the dictionary's end.
fn cursor_key(dict: &SortedDict, ordinal: u32, scratch: &mut Vec<u8>) -> io::Result<Option<String>> {
    if ordinal >= dict.len() {
        return Ok(None);
    }
    let key = dict.key_of(ordinal, scratch)?;
    Ok(Some(key.to_string()))
}
