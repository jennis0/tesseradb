//! `entities/terms/` — the **transpose of the term postings**: one entity's own term ordinals,
//! read back in one probe instead of a sweep over the dictionary (contracts §2.4).
//!
//! # Why a second copy of a relation the bundle already holds
//!
//! Labels are stored term-major, as postings: `terms/postings.arrow` answers *which entities
//! carry term `t`*, which is the question the mask is built from and the only question the
//! request path used to ask. Two questions run the other way, and neither is answerable from the
//! postings without walking every term in the dictionary:
//!
//! * **The drill-down's `labels` array** (`records-and-search.md`, decision 0114) — the
//!   intersection of an entity's own terms with the asking session's *satisfied* set. The
//!   intersection is taken here, server-side, and only the survivors are resolved to descriptors
//!   and served; nothing outside the session's own authority reaches the response.
//! * **The join rule's label arm** (`views.md` §4) — a second batch naming an already-flushed
//!   entity under a different label must be a `409`, and that comparison needs the entity's
//!   **full** set, which is a server-side quantity and never served.
//!
//! `terms/pairs.parquet` is the same relation flat, and is not this: it is sorted term-major,
//! optional for a serving deployment, and read at build cadence by the oracle alone (contracts
//! §2.4). A request path cannot be built on a file a deployment may legitimately omit.
//!
//! # The format
//!
//! Three files per layer, the same **has-row rank** addressing the record blob uses (records §3,
//! review B5) and for the same reason: an entity with no term list costs nothing anywhere.
//!
//! ```text
//! hasrow.roaring   portable Roaring over the entity ids this layer holds a list for
//! offsets.u32      (cardinality + 1) u32 LE, ascending, [0] = 0 — start offsets into terms.u32
//! terms.u32        u32 LE term ordinals, each entity's slice strictly ascending
//! ```
//!
//! An entity's rank in `hasrow` indexes `offsets`; its terms are `terms[offsets[r]..offsets[r+1]]`.
//! **An empty slice is a real answer** — an item may legitimately carry zero terms — and is
//! distinct from an entity absent from `hasrow`, which this layer says nothing about.
//!
//! # The ordinals are bundle-relative and survive every rewrite
//!
//! A stored ordinal is a position in the concatenation of `dict_extents` in listed order
//! ([`tessera_authz::Dict::load`]), which is append-only: `coalesce_dict_extents` replaces a
//! *contiguous* window with the same records in the same order, and the compaction fold carries
//! the dictionary forward by a hard link, never renumbered and never shrunk (compaction §3 pass
//! 4b). So nothing that rewrites the corpus rewrites these numbers, and this artefact needs no
//! remap at coalesce or at fold — only the retirement of the entities `D₀` names.
//!
//! # Layers are disjoint, so the stack is a probe and not a merge
//!
//! The build writes the base; each flush writes an extent for the entities **it** minted, which
//! by I9 no other layer holds (an entity id is allocated once, and a *joining* row contributes no
//! entity-space fact at all — `FlushPlan::entity_space_items`). So the layer whose has-row bitmap
//! contains the entity answers, and no other layer can contradict it — [`EntityTermsStack`] is
//! the same five lines `RecordStack` is, for the same reason.
//!
//! # Fail-closed
//!
//! A short `offsets`, a `terms` file that does not end where the last offset says, and a
//! descending or out-of-range offset pair are all [`StoreError::InvalidEntityTerms`] — never a
//! truncated answer. The two ends are checked at open and each pair at the read that uses it,
//! which is O(1) both times: walking every offset at open would be a 4 GB sequential read at 10⁹,
//! on the path the external-ID sidecar was deliberately made lazy to keep clear.
//!
//! A truncated list would under-report an entity's labels, which on the write path is a **409 that
//! does not fire**: a re-label accepted through a second view's row, with no overlay entry. On the
//! read path it would only hide a label the viewer holds, which is the harmless direction — but
//! the two share this reader, so it is held to the write path's standard.
//!
//! # The coalesce merges the extents, and it is a concatenation
//!
//! An entity-space coalesce takes a contiguous window of `entity_terms_extents` and replaces it
//! with one extent ([`coalesce_entity_terms_extents`]) — the **record blob's** axis exactly: three
//! files, has-row addressed, disjoint in entity space, one window of one list spliced back at the
//! window's position. Without it the layers accumulate one per flush until the next fold, and the
//! reader pays file handles and a base-plus-linear probe per lookup.
//!
//! **A merge here needs no remap and no dictionary**, which is what makes it a concatenation
//! rather than the keyword axis's renumbering: the ordinals are dictionary positions, preserved by
//! every rewrite for the reason above, and the layers are disjoint by **I9**. So the merge walks
//! the inputs' entity sets in ascending order and copies each list verbatim — the same bytes in
//! the same order, one file set instead of *k*.
//!
//! **It retires nothing.** There is no tombstone parameter to pass and no route to a deletion:
//! Rule S and Rule F are the fold's (write-path §5.4), and an entity awaiting a deletion keeps
//! its term list across a coalesce, hidden by the read gate and retired at the fold that executes
//! it.
//!
//! **No error detail here names a descriptor**, only ordinals, lengths and paths: these strings
//! reach an operator log, and a descriptor is a compartment name.
//!
//! **The reader's details name no entity either** — the external-ID sidecar's rule, at the same
//! standard and for its reason: a read failure is reachable from a request, contracts §4 has the
//! byte-scanner sweep logs as well as payloads for entity ids (**I10**), and a corrupt layer is a
//! systematic build or flush fault whose file and inconsistency shape are what an operator needs.
//! The **writer's** three refusals do name the entity, and that is the one place it belongs: they
//! fire only on a caller feeding this type out of order, which no input can reach — both producers
//! walk entity space ascending — so there is no request behind them and the slot is the whole
//! diagnostic.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use croaring::{Bitmap, Portable};
use memmap2::Mmap;

use crate::error::{Result, StoreError};

/// The has-row bitmap's file name, in a layer's directory.
pub const ENTITY_TERMS_HASROW_FILE: &str = "hasrow.roaring";
/// The offsets array's file name, in a layer's directory.
pub const ENTITY_TERMS_OFFSETS_FILE: &str = "offsets.u32";
/// The term ordinals' file name, in a layer's directory.
pub const ENTITY_TERMS_TERMS_FILE: &str = "terms.u32";

/// The base layer's directory, relative to a partition directory.
pub const ENTITY_TERMS_DIR: &str = "entities/terms";

/// One extent's three files, as a manifest's `entity_terms_extents` entry names them.
#[derive(Debug, Clone)]
pub struct EntityTermsExtentPaths {
    pub hasrow: PathBuf,
    pub offsets: PathBuf,
    pub terms: PathBuf,
}

/// Writes one layer — base or extent — streaming, in ascending entity order.
///
/// **Streaming, not buffered, and that is the sizing argument.** At 10⁹ entities carrying a
/// handful of terms each, `terms.u32` is tens of GB; an implementation that accumulated the
/// relation and serialised it at `finish` would hold the corpus in anonymous memory. The has-row
/// bitmap is the one thing kept in memory, and Roaring over a dense ascending set is a run
/// container per 2¹⁶ block.
pub struct EntityTermsWriter {
    hasrow_path: PathBuf,
    offsets_path: PathBuf,
    terms_path: PathBuf,
    hasrow: Bitmap,
    offsets: BufWriter<File>,
    terms: BufWriter<File>,
    /// The running total, and therefore the next offset to emit.
    written: u32,
    /// The last entity pushed, so a caller feeding them out of order fails here rather than
    /// producing a layer whose ranks name the wrong lists.
    last: Option<u32>,
}

impl EntityTermsWriter {
    /// Create the three files under `dir`, by their conventional names — the base layer's shape.
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        Self::create_at(
            &dir.join(ENTITY_TERMS_HASROW_FILE),
            &dir.join(ENTITY_TERMS_OFFSETS_FILE),
            &dir.join(ENTITY_TERMS_TERMS_FILE),
        )
    }

    /// Create the three files by explicit path — an extent's shape, whose three files share one
    /// directory with every other extent's and are distinguished by a `<seg_id>.` prefix, exactly
    /// as the record blob's extents are.
    pub fn create_at(hasrow_path: &Path, offsets_path: &Path, terms_path: &Path) -> Result<Self> {
        let mut offsets =
            BufWriter::new(File::create(offsets_path).map_err(|e| io(offsets_path, e))?);
        // The leading zero, written up front: `offsets` has one more element than the bitmap has
        // members, and the reader reads `[r]` and `[r+1]` unconditionally.
        offsets
            .write_all(&0u32.to_le_bytes())
            .map_err(|e| io(offsets_path, e))?;
        let terms = BufWriter::new(File::create(terms_path).map_err(|e| io(terms_path, e))?);
        Ok(Self {
            hasrow_path: hasrow_path.to_path_buf(),
            offsets_path: offsets_path.to_path_buf(),
            terms_path: terms_path.to_path_buf(),
            hasrow: Bitmap::new(),
            offsets,
            terms,
            written: 0,
            last: None,
        })
    }

    /// Record `entity`'s term list. `terms` must be **strictly ascending** (a label set is a set),
    /// and entities must arrive strictly ascending. Both are refused rather than repaired: a
    /// duplicate or an out-of-order id here is a defect in the caller's own transpose, and
    /// quietly sorting it would leave two layers disagreeing about one entity with nothing to
    /// notice.
    ///
    /// An empty `terms` is accepted and is not the same as never calling this: it records that
    /// this layer holds the entity and that it carries no term.
    pub fn push(&mut self, entity: u32, terms: &[u32]) -> Result<()> {
        if let Some(last) = self.last {
            if entity <= last {
                return Err(self.invalid(format!(
                    "entity {entity} pushed after {last}; a layer is written in strictly \
                     ascending entity order because its ranks address it"
                )));
            }
        }
        if terms.windows(2).any(|w| w[0] >= w[1]) {
            return Err(self.invalid(format!(
                "entity {entity}'s term list is not strictly ascending; a label set is a set"
            )));
        }
        let next = self
            .written
            .checked_add(u32::try_from(terms.len()).unwrap_or(u32::MAX))
            .ok_or_else(|| {
                self.invalid(format!(
                    "the layer's term count passes u32::MAX at entity {entity}"
                ))
            })?;
        for term in terms {
            self.terms
                .write_all(&term.to_le_bytes())
                .map_err(|e| io(&self.terms_path, e))?;
        }
        self.offsets
            .write_all(&next.to_le_bytes())
            .map_err(|e| io(&self.offsets_path, e))?;
        self.hasrow.add(entity);
        self.written = next;
        self.last = Some(entity);
        Ok(())
    }

    /// Flush and close, returning the three paths written, in `(hasrow, offsets, terms)` order.
    pub fn finish(mut self) -> Result<Vec<PathBuf>> {
        self.offsets
            .flush()
            .map_err(|e| io(&self.offsets_path, e))?;
        self.terms.flush().map_err(|e| io(&self.terms_path, e))?;
        // `run_optimize` before serialising, as the corpus's other Roaring writers do: a layer's
        // entities are an ascending, usually contiguous, run of ids.
        self.hasrow.run_optimize();
        std::fs::write(&self.hasrow_path, self.hasrow.serialize::<Portable>())
            .map_err(|e| io(&self.hasrow_path, e))?;
        Ok(vec![self.hasrow_path, self.offsets_path, self.terms_path])
    }

    fn invalid(&self, detail: String) -> StoreError {
        StoreError::InvalidEntityTerms {
            path: self.terms_path.clone(),
            detail,
        }
    }
}

/// One opened layer.
pub struct EntityTerms {
    hasrow: Bitmap,
    offsets: Mmap,
    terms: Mmap,
    /// Carried for error messages only.
    dir: PathBuf,
}

impl EntityTerms {
    /// Open the base layer from its directory.
    pub fn open_dir(dir: &Path) -> Result<Self> {
        Self::open(
            &dir.join(ENTITY_TERMS_HASROW_FILE),
            &dir.join(ENTITY_TERMS_OFFSETS_FILE),
            &dir.join(ENTITY_TERMS_TERMS_FILE),
        )
    }

    /// Open one layer from its three files, checking every length and the offsets' monotonicity —
    /// see the module doc for why this is fail-closed rather than tolerant.
    pub fn open(hasrow_path: &Path, offsets_path: &Path, terms_path: &Path) -> Result<Self> {
        let dir = hasrow_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let hasrow_bytes = std::fs::read(hasrow_path).map_err(|e| io(hasrow_path, e))?;
        let hasrow = Bitmap::try_deserialize::<Portable>(&hasrow_bytes).ok_or_else(|| {
            StoreError::InvalidEntityTerms {
                path: hasrow_path.to_path_buf(),
                detail: "the has-row bitmap is not portable Roaring".to_string(),
            }
        })?;
        let offsets = map(offsets_path)?;
        let terms = map(terms_path)?;

        let card = hasrow.cardinality();
        let want_offsets = (card + 1)
            .checked_mul(4)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| StoreError::InvalidEntityTerms {
                path: dir.clone(),
                detail: format!("a has-row cardinality of {card} overflows the offsets length"),
            })?;
        if offsets.len() != want_offsets {
            return Err(StoreError::InvalidEntityTerms {
                path: offsets_path.to_path_buf(),
                detail: format!(
                    "{} bytes for {card} entities; a layer owes one offset per entity plus a \
                     terminator, i.e. {want_offsets} bytes",
                    offsets.len()
                ),
            });
        }
        if terms.len() % 4 != 0 {
            return Err(StoreError::InvalidEntityTerms {
                path: terms_path.to_path_buf(),
                detail: format!("{} bytes is not a whole number of u32", terms.len()),
            });
        }
        // **The ends, not the whole array.** The first offset must be 0 and the last must name
        // exactly the terms file's length; every offset between them is checked at the read that
        // uses it (`terms_of`), which is the same fail-closed answer at the point where a bad pair
        // could produce a wrong one. Walking all of them here would be a sequential read of 4 B per
        // entity — 4 GB at 10⁹ — at every `Engine::open`, which is the startup cost the external-ID
        // sidecar was deliberately made lazy to avoid; the check that catches a truncation is the
        // length equality below, and it is O(1).
        let first = read_u32(&offsets, 0);
        if first != 0 {
            return Err(StoreError::InvalidEntityTerms {
                path: offsets_path.to_path_buf(),
                detail: format!("the first offset is {first}, not 0"),
            });
        }
        let last = read_u32(&offsets, card as usize);
        if (last as usize) * 4 != terms.len() {
            return Err(StoreError::InvalidEntityTerms {
                path: dir.clone(),
                detail: format!(
                    "the last offset names {last} term ordinals but the terms file holds {}",
                    terms.len() / 4
                ),
            });
        }
        Ok(Self {
            hasrow,
            offsets,
            terms,
            dir,
        })
    }

    /// How many entities this layer holds a list for — what the writer put in it, read back.
    ///
    /// The one caller is this module's own round-trip test, which is the point: nothing on a
    /// request path asks a layer its size, and a method that existed for a *reader* would be an
    /// affordance for the corpus-sized question this artefact deliberately does not answer.
    pub fn len(&self) -> u64 {
        self.hasrow.cardinality()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `entity`'s term ordinals, ascending, or `None` where this layer holds no list for it.
    ///
    /// **Decoded rather than transmuted.** A `&[u32]` over a mapping would depend on the file
    /// being `u32`-aligned and the host being little-endian; a list is a handful of ordinals read
    /// at drill-down cadence, so the explicit decode costs nothing worth the two unstated
    /// premises.
    pub fn terms_of(&self, entity: u32) -> Result<Option<Vec<u32>>> {
        if !self.hasrow.contains(entity) {
            return Ok(None);
        }
        let rank = (self.hasrow.rank(entity) - 1) as usize;
        let start = read_u32(&self.offsets, rank) as usize;
        let end = read_u32(&self.offsets, rank + 1) as usize;
        // **The pair is checked here rather than at open** — see [`EntityTerms::open`] for why the
        // whole array is not walked. A descending pair or one past the terms file is a refusal and
        // never a truncated list: a short label set on the write path is a `409` that does not
        // fire, which is the fail-open direction.
        if end < start || end * 4 > self.terms.len() {
            return Err(StoreError::InvalidEntityTerms {
                path: self.dir.clone(),
                detail: format!(
                    "the offsets at rank {rank} name [{start}, {end}) over a terms file of \
                     {} ordinals",
                    self.terms.len() / 4
                ),
            });
        }
        Ok(Some(
            self.terms[start * 4..end * 4]
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        ))
    }
}

/// Merge `inputs` — a coalesce's window of extents — into one layer at the three given paths,
/// returning how many entities it holds.
///
/// **A concatenation with bookkeeping, not a merge with a resolution rule.** The layers are
/// disjoint in entity space (**I9**: an entity id is allocated once, and the flush that minted it
/// wrote the only layer that holds its list), so no entity appears twice and no input can
/// contradict another — the output is each entity's own list, byte-for-byte, gathered in ascending
/// entity order. Term ordinals are positions in the concatenated dictionary extents and are
/// preserved by every rewrite of the corpus (see this module's doc), so nothing is remapped.
///
/// **Byte-deterministic for a given input set**: the output is a pure function of the entity sets
/// and the lists, and the order of the walk is the ascending entity order the format already
/// requires. Two merges of the same inputs produce the same three files.
///
/// A repeated entity is a **refusal**, not a resolution. Disjointness is a property of the
/// writers, and this is the one place a violation of it could be silently collapsed into a layer
/// that answered one flush's labels for another's entity — so it fails the pass instead.
///
/// **Nothing is retired here** (Rule S / Rule F, write-path §5.4): there is no tombstone
/// parameter, and an entity awaiting a deletion keeps its list until the fold executes it.
pub fn coalesce_entity_terms_extents(
    inputs: &[&EntityTerms],
    hasrow_path: &Path,
    offsets_path: &Path,
    terms_path: &Path,
) -> Result<u64> {
    if let Some(parent) = hasrow_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let mut writer = EntityTermsWriter::create_at(hasrow_path, offsets_path, terms_path)?;
    let mut cursors: Vec<std::iter::Peekable<croaring::bitmap::BitmapIterator<'_>>> = inputs
        .iter()
        .map(|layer| layer.hasrow.iter().peekable())
        .collect();
    let mut written = 0u64;
    loop {
        // The least unconsumed entity across the inputs, and the layer holding it. `k` is a
        // coalesce window — eight by default — so a scan per entity is cheaper than a heap and
        // carries the duplicate check for nothing.
        let mut least: Option<(usize, u32)> = None;
        for (index, cursor) in cursors.iter_mut().enumerate() {
            let Some(&entity) = cursor.peek() else {
                continue;
            };
            match least {
                Some((held, at)) if entity == at => {
                    return Err(StoreError::InvalidEntityTerms {
                        path: terms_path.to_path_buf(),
                        detail: format!(
                            "inputs {held} and {index} both hold a term list for one entity; the \
                             layers of this family are disjoint (I9) and merging them would \
                             publish one flush's labels under another's"
                        ),
                    });
                }
                Some((_, at)) if entity > at => {}
                _ => least = Some((index, entity)),
            }
        }
        let Some((index, entity)) = least else { break };
        cursors[index].next();
        let terms = inputs[index]
            .terms_of(entity)?
            .expect("the layer's own has-row bitmap named this entity");
        writer.push(entity, &terms)?;
        written += 1;
    }
    drop(cursors);
    writer.finish()?;
    Ok(written)
}

/// The base layer plus every flush extent, probed as one.
///
/// `Arc` per layer, and appended rather than reopened after a flush, for the reason
/// [`crate::sidecar`] and the record stack both give: the base is the largest artefact of its
/// family and remapping it at every publication would undo a generation's cheap succession.
pub struct EntityTermsStack {
    layers: Vec<Arc<EntityTerms>>,
}

impl EntityTermsStack {
    /// Open the stack. `base` is the partition's `entities/terms` directory, `None` for a bundle
    /// whose build wrote no layer at all; `extents` are the manifest's `entity_terms_extents`,
    /// oldest first. Any layer that fails its open refuses the whole stack.
    pub fn open(base: Option<&Path>, extents: &[EntityTermsExtentPaths]) -> Result<Self> {
        let mut layers = Vec::with_capacity(extents.len() + 1);
        if let Some(dir) = base {
            layers.push(Arc::new(EntityTerms::open_dir(dir)?));
        }
        for extent in extents {
            layers.push(Arc::new(EntityTerms::open(
                &extent.hasrow,
                &extent.offsets,
                &extent.terms,
            )?));
        }
        Ok(Self { layers })
    }

    /// A stack holding nothing — every probe misses.
    pub fn empty() -> Self {
        Self { layers: Vec::new() }
    }

    /// This stack with `extents` appended: the successor generation's, after a flush.
    pub fn with_extents(&self, extents: &[EntityTermsExtentPaths]) -> Result<Self> {
        let mut layers = self.layers.clone();
        for extent in extents {
            layers.push(Arc::new(EntityTerms::open(
                &extent.hasrow,
                &extent.offsets,
                &extent.terms,
            )?));
        }
        Ok(Self { layers })
    }

    /// `entity`'s term ordinals, ascending, or `None` where no layer holds a list for it.
    ///
    /// `None` is **not** "this entity carries no terms" — that is `Some(vec![])`. It is "no layer
    /// published one", which for a live entity means the transpose is behind its postings, and
    /// every caller treats it as *unknown* rather than as *empty*.
    pub fn terms_of(&self, entity: u32) -> Result<Option<Vec<u32>>> {
        for layer in &self.layers {
            if let Some(terms) = layer.terms_of(entity)? {
                return Ok(Some(terms));
            }
        }
        Ok(None)
    }

    /// Every entity any layer holds a list for, ascending — the fold's walk. The layers are
    /// disjoint by **I9**, so this is their concatenation in ascending order rather than a merge
    /// with a dedup; it is built as a union bitmap anyway, because "disjoint" is a property of the
    /// writers and this is the one place a violation would produce a layer with a repeated entity
    /// instead of an error.
    /// Returned as a bitmap rather than an iterator so the caller iterates it in place: at 10⁹
    /// the materialised `Vec<u32>` is 4 GB, where the Roaring union of dense ascending runs is a
    /// few containers.
    pub fn entity_set(&self) -> Bitmap {
        let mut all = Bitmap::new();
        for layer in &self.layers {
            all |= &layer.hasrow;
        }
        all.run_optimize();
        all
    }

    /// How many layers the stack holds — the base counts as one.
    pub fn layers(&self) -> usize {
        self.layers.len()
    }
}

/// The `index`-th `u32` of a mapped LE array. The caller has already established that the array is
/// long enough — at open for the two ends, and by the has-row cardinality for a rank.
fn read_u32(map: &Mmap, index: usize) -> u32 {
    let at = index * 4;
    u32::from_le_bytes([map[at], map[at + 1], map[at + 2], map[at + 3]])
}

fn map(path: &Path) -> Result<Mmap> {
    let file = File::open(path).map_err(|e| io(path, e))?;
    // SAFETY: the file is a published, immutable bundle artefact (contracts §2.1 — every file but
    // `CURRENT` is immutable and a prefix grows only by whole new files), so nothing truncates it
    // under the mapping. Identical justification to the external-ID sidecar's extents.
    unsafe { Mmap::map(&file) }.map_err(|e| io(path, e))
}

fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(rows: &[(u32, Vec<u32>)]) -> (tempfile::TempDir, EntityTerms) {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = EntityTermsWriter::create(dir.path()).unwrap();
        for (entity, terms) in rows {
            writer.push(*entity, terms).unwrap();
        }
        writer.finish().unwrap();
        let opened = EntityTerms::open_dir(dir.path()).unwrap();
        (dir, opened)
    }

    /// The whole of the format's contract, in one assertion each: a sparse entity set, a
    /// variable-length list, and the empty list that is a value rather than an absence.
    #[test]
    fn a_layer_answers_each_entity_its_own_list() {
        let rows = vec![
            (0u32, vec![0u32, 7, 9]),
            (5, vec![]),
            (9, vec![3]),
            (1_000_000, vec![0, 1, 2, 3, 4]),
        ];
        let (_dir, layer) = round_trip(&rows);
        for (entity, terms) in &rows {
            assert_eq!(layer.terms_of(*entity).unwrap().as_ref(), Some(terms));
        }
        // An entity the layer never held is unknown, and is not the empty list.
        assert_eq!(layer.terms_of(1).unwrap(), None);
        assert_eq!(layer.terms_of(4).unwrap(), None);
        assert_eq!(layer.len(), 4);
    }

    /// **`Some(vec![])` and `None` are different answers**, and conflating them is what would
    /// make the join rule's label arm compare an entity's labels against nothing and pass.
    #[test]
    fn an_empty_list_is_a_value_and_an_absent_entity_is_not() {
        let (_dir, layer) = round_trip(&[(2u32, vec![])]);
        assert_eq!(layer.terms_of(2).unwrap(), Some(Vec::new()));
        assert_eq!(layer.terms_of(3).unwrap(), None);
    }

    #[test]
    fn the_writer_refuses_a_descending_entity_and_an_unsorted_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = EntityTermsWriter::create(dir.path()).unwrap();
        writer.push(4, &[1, 2]).unwrap();
        assert!(writer.push(4, &[3]).is_err(), "an entity is pushed once");
        assert!(writer.push(3, &[3]).is_err(), "and in ascending order");
        assert!(
            writer.push(5, &[2, 2]).is_err(),
            "a label set is a set: a repeat is refused, not deduplicated"
        );
        assert!(writer.push(6, &[9, 1]).is_err(), "and it is ascending");
    }

    /// A truncated `terms.u32` must refuse the open, not answer a shorter list. This is the
    /// mutation that matters: the shorter list is a label the write path's 409 would not see.
    #[test]
    fn a_truncated_terms_file_refuses_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = EntityTermsWriter::create(dir.path()).unwrap();
        writer.push(0, &[1, 2, 3]).unwrap();
        writer.finish().unwrap();
        let terms = dir.path().join(ENTITY_TERMS_TERMS_FILE);
        let bytes = std::fs::read(&terms).unwrap();
        std::fs::write(&terms, &bytes[..bytes.len() - 4]).unwrap();
        assert!(matches!(
            EntityTerms::open_dir(dir.path()),
            Err(StoreError::InvalidEntityTerms { .. })
        ));
    }

    /// And so must an offsets array that has lost its terminator — the reader indexes `r + 1`.
    #[test]
    fn a_short_offsets_array_refuses_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = EntityTermsWriter::create(dir.path()).unwrap();
        writer.push(0, &[1]).unwrap();
        writer.push(1, &[2]).unwrap();
        writer.finish().unwrap();
        let offsets = dir.path().join(ENTITY_TERMS_OFFSETS_FILE);
        let bytes = std::fs::read(&offsets).unwrap();
        std::fs::write(&offsets, &bytes[..bytes.len() - 4]).unwrap();
        assert!(matches!(
            EntityTerms::open_dir(dir.path()),
            Err(StoreError::InvalidEntityTerms { .. })
        ));
    }

    /// The stack probes its layers and the first holder answers — disjointness by I9 is what
    /// makes order irrelevant, so this also pins that a base and an extent never both hold one.
    #[test]
    fn the_stack_answers_from_whichever_layer_holds_the_entity() {
        let base = tempfile::tempdir().unwrap();
        let mut writer = EntityTermsWriter::create(base.path()).unwrap();
        writer.push(0, &[1]).unwrap();
        writer.push(1, &[2, 3]).unwrap();
        writer.finish().unwrap();

        let extent = tempfile::tempdir().unwrap();
        let mut writer = EntityTermsWriter::create(extent.path()).unwrap();
        writer.push(7, &[4]).unwrap();
        writer.finish().unwrap();

        let paths = EntityTermsExtentPaths {
            hasrow: extent.path().join(ENTITY_TERMS_HASROW_FILE),
            offsets: extent.path().join(ENTITY_TERMS_OFFSETS_FILE),
            terms: extent.path().join(ENTITY_TERMS_TERMS_FILE),
        };
        let stack =
            EntityTermsStack::open(Some(base.path()), std::slice::from_ref(&paths)).unwrap();
        assert_eq!(stack.terms_of(0).unwrap(), Some(vec![1]));
        assert_eq!(stack.terms_of(1).unwrap(), Some(vec![2, 3]));
        assert_eq!(stack.terms_of(7).unwrap(), Some(vec![4]));
        assert_eq!(stack.terms_of(6).unwrap(), None);
        assert_eq!(stack.layers(), 2);

        // The successor generation's stack, after a flush: the base is shared, not reopened.
        let grown = EntityTermsStack::open(Some(base.path()), &[])
            .unwrap()
            .with_extents(std::slice::from_ref(&paths))
            .unwrap();
        assert_eq!(grown.terms_of(7).unwrap(), Some(vec![4]));
    }

    /// **A layer holding nothing must still open.** A flush that published only joining rows
    /// mints no entity and writes an empty layer — three files, one of them zero bytes — and a
    /// reader that refused it would fail the whole generation's open on a legitimate publication.
    #[test]
    fn an_empty_layer_opens_and_answers_nothing() {
        let (_dir, layer) = round_trip(&[]);
        assert_eq!(layer.len(), 0);
        assert!(layer.is_empty());
        assert_eq!(layer.terms_of(0).unwrap(), None);
    }

    #[test]
    fn an_empty_stack_answers_nothing_rather_than_failing() {
        assert_eq!(EntityTermsStack::empty().terms_of(0).unwrap(), None);
    }

    /// A generated set of disjoint layers, written as a coalesce window would find them: each
    /// layer's entities strictly above the last's, lists of varying length, empty lists included.
    struct Built {
        dir: tempfile::TempDir,
        rows: Vec<(u32, Vec<u32>)>,
    }

    fn disjoint_layers(count: usize) -> Vec<Built> {
        let mut out = Vec::new();
        let mut next_entity = 0u32;
        for layer in 0..count {
            let dir = tempfile::tempdir().unwrap();
            let mut rows: Vec<(u32, Vec<u32>)> = Vec::new();
            // A deliberately irregular shape per layer: a gap, a run, an empty list, a long one.
            for i in 0..(3 + layer % 4) {
                next_entity += 1 + (i as u32 % 3);
                let len = (layer * 7 + i * 3) % 5;
                let terms: Vec<u32> = (0..len).map(|t| (t as u32) * 2 + layer as u32).collect();
                rows.push((next_entity, terms));
            }
            next_entity += 17;
            let mut writer = EntityTermsWriter::create(dir.path()).unwrap();
            for (entity, terms) in &rows {
                writer.push(*entity, terms).unwrap();
            }
            writer.finish().unwrap();
            out.push(Built { dir, rows });
        }
        out
    }

    fn paths_of(dir: &Path) -> EntityTermsExtentPaths {
        EntityTermsExtentPaths {
            hasrow: dir.join(ENTITY_TERMS_HASROW_FILE),
            offsets: dir.join(ENTITY_TERMS_OFFSETS_FILE),
            terms: dir.join(ENTITY_TERMS_TERMS_FILE),
        }
    }

    fn merge_into(dir: &Path, layers: &[&EntityTerms]) -> u64 {
        coalesce_entity_terms_extents(
            layers,
            &dir.join(ENTITY_TERMS_HASROW_FILE),
            &dir.join(ENTITY_TERMS_OFFSETS_FILE),
            &dir.join(ENTITY_TERMS_TERMS_FILE),
        )
        .unwrap()
    }

    /// **The merge answers what the stack answered**, entity for entity, and it is the only claim
    /// the coalesce makes: fewer layers, identical answers. Over a generated set of layers rather
    /// than one hand-written pair, because the bookkeeping the merge does — ranks, offsets, the
    /// empty list that is a value — is exactly what a single tidy example would not exercise.
    #[test]
    fn a_merge_of_disjoint_extents_answers_what_the_layered_read_answered() {
        let built = disjoint_layers(6);
        let opened: Vec<EntityTerms> = built
            .iter()
            .map(|built| EntityTerms::open_dir(built.dir.path()).unwrap())
            .collect();
        let extents: Vec<EntityTermsExtentPaths> =
            built.iter().map(|b| paths_of(b.dir.path())).collect();
        let stack = EntityTermsStack::open(None, &extents).unwrap();

        let out = tempfile::tempdir().unwrap();
        let refs: Vec<&EntityTerms> = opened.iter().collect();
        let written = merge_into(out.path(), &refs);
        let merged = EntityTermsStack::open(None, &[paths_of(out.path())]).unwrap();

        let expected: u64 = built.iter().map(|b| b.rows.len() as u64).sum();
        assert_eq!(written, expected, "every entity of every input is carried");
        assert_eq!(merged.layers(), 1, "six layers became one");
        assert_eq!(merged.entity_set(), stack.entity_set());

        // Every entity the inputs hold, and a band of ids around them that they do not: an
        // absence must stay an absence, or a coalesce would turn "unknown" into "no labels".
        let highest = stack.entity_set().maximum().unwrap();
        for entity in 0..=highest + 8 {
            assert_eq!(
                merged.terms_of(entity).unwrap(),
                stack.terms_of(entity).unwrap(),
                "entity {entity} answers differently after the merge"
            );
        }
    }

    /// **Two merges of the same inputs are byte-equal.** The bundle's identity is its files'
    /// digests, and a merge whose output depended on iteration order or on a hash seed would give
    /// two nodes coalescing the same window two different bundles.
    #[test]
    fn a_merge_is_byte_deterministic_for_its_input_set() {
        let built = disjoint_layers(4);
        let opened: Vec<EntityTerms> = built
            .iter()
            .map(|built| EntityTerms::open_dir(built.dir.path()).unwrap())
            .collect();
        let refs: Vec<&EntityTerms> = opened.iter().collect();

        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert_eq!(
            merge_into(first.path(), &refs),
            merge_into(second.path(), &refs)
        );
        for name in [
            ENTITY_TERMS_HASROW_FILE,
            ENTITY_TERMS_OFFSETS_FILE,
            ENTITY_TERMS_TERMS_FILE,
        ] {
            assert_eq!(
                std::fs::read(first.path().join(name)).unwrap(),
                std::fs::read(second.path().join(name)).unwrap(),
                "{name} differs between two merges of one input set"
            );
        }
    }

    /// **A repeated entity is refused, not resolved.** Disjointness is a property of the writers
    /// (I9), and this is the one place a violation could be collapsed into a layer answering one
    /// flush's labels for another flush's entity.
    #[test]
    fn a_merge_refuses_inputs_that_share_an_entity() {
        let (_a, first) = round_trip(&[(3u32, vec![1])]);
        let (_b, second) = round_trip(&[(3u32, vec![2])]);
        let out = tempfile::tempdir().unwrap();
        assert!(matches!(
            coalesce_entity_terms_extents(
                &[&first, &second],
                &out.path().join(ENTITY_TERMS_HASROW_FILE),
                &out.path().join(ENTITY_TERMS_OFFSETS_FILE),
                &out.path().join(ENTITY_TERMS_TERMS_FILE),
            ),
            Err(StoreError::InvalidEntityTerms { .. })
        ));
    }

    /// The fail-closed opens hold for the merged artefact exactly as for a flush's — the reader
    /// cannot tell the two apart, and this pins that a coalesce has not produced a shape the
    /// truncation checks read as whole.
    #[test]
    fn a_truncated_merged_extent_refuses_the_open() {
        let built = disjoint_layers(3);
        let opened: Vec<EntityTerms> = built
            .iter()
            .map(|built| EntityTerms::open_dir(built.dir.path()).unwrap())
            .collect();
        let out = tempfile::tempdir().unwrap();
        merge_into(out.path(), &opened.iter().collect::<Vec<_>>());
        assert!(EntityTerms::open_dir(out.path()).is_ok(), "whole, it opens");

        for name in [ENTITY_TERMS_TERMS_FILE, ENTITY_TERMS_OFFSETS_FILE] {
            let path = out.path().join(name);
            let whole = std::fs::read(&path).unwrap();
            std::fs::write(&path, &whole[..whole.len() - 4]).unwrap();
            assert!(
                matches!(
                    EntityTerms::open_dir(out.path()),
                    Err(StoreError::InvalidEntityTerms { .. })
                ),
                "a truncated {name} must refuse the open"
            );
            std::fs::write(&path, &whole).unwrap();
        }
    }

    /// A window of empty layers — a run of flushes that minted nothing — merges to an empty layer
    /// rather than refusing. The pass fires on the entry count, not on the bytes.
    #[test]
    fn a_merge_of_empty_layers_is_an_empty_layer() {
        let (_a, first) = round_trip(&[]);
        let (_b, second) = round_trip(&[]);
        let out = tempfile::tempdir().unwrap();
        assert_eq!(merge_into(out.path(), &[&first, &second]), 0);
        let merged = EntityTerms::open_dir(out.path()).unwrap();
        assert!(merged.is_empty());
        assert_eq!(merged.terms_of(0).unwrap(), None);
    }
}
