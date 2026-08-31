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
//! Every length and offset is checked at open. A short `offsets`, a non-monotone one, or a
//! `terms` file that does not end where the last offset says are all
//! [`StoreError::InvalidEntityTerms`] — never a truncated answer. A truncated list here would
//! under-report an entity's labels, which on the write path is a **409 that does not fire**: a
//! re-label accepted through a second view's row, with no overlay entry. On the read path it
//! would only hide a label the viewer holds, which is the harmless direction — but the two share
//! this reader, so it is held to the write path's standard.
//!
//! **No error detail here names a descriptor**, only ordinals, lengths and paths: these strings
//! reach an operator log, and a descriptor is a compartment name.

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
        self.offsets.flush().map_err(|e| io(&self.offsets_path, e))?;
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
        // Monotone, starting at zero and ending at the term file's own length. Checked once here
        // so `terms_of` can slice without re-deriving the bound per read; the cost is one
        // sequential pass over 4 bytes per entity at open, which is the same pass the digest
        // verification already makes over every file in the bundle.
        let mut previous = 0u32;
        for index in 0..=card {
            let at = (index as usize) * 4;
            let value = u32::from_le_bytes([
                offsets[at],
                offsets[at + 1],
                offsets[at + 2],
                offsets[at + 3],
            ]);
            if index == 0 && value != 0 {
                return Err(StoreError::InvalidEntityTerms {
                    path: offsets_path.to_path_buf(),
                    detail: format!("the first offset is {value}, not 0"),
                });
            }
            if value < previous {
                return Err(StoreError::InvalidEntityTerms {
                    path: offsets_path.to_path_buf(),
                    detail: format!(
                        "offset {index} is {value}, below its predecessor {previous}; the array \
                         addresses slices and must not decrease"
                    ),
                });
            }
            previous = value;
        }
        if (previous as usize) * 4 != terms.len() {
            return Err(StoreError::InvalidEntityTerms {
                path: dir.clone(),
                detail: format!(
                    "the last offset names {previous} term ordinals but the terms file holds {}",
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

    /// Does this layer hold a list for `entity`?
    pub fn holds(&self, entity: u32) -> bool {
        self.hasrow.contains(entity)
    }

    /// How many entities this layer holds a list for.
    pub fn len(&self) -> u64 {
        self.hasrow.cardinality()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entities this layer holds, for a fold's merge.
    pub fn entities(&self) -> impl Iterator<Item = u32> + '_ {
        self.hasrow.iter()
    }

    /// `entity`'s term ordinals, ascending, or `None` where this layer holds no list for it.
    ///
    /// **Decoded rather than transmuted.** A `&[u32]` over a mapping would depend on the file
    /// being `u32`-aligned and the host being little-endian; a list is a handful of ordinals read
    /// at drill-down cadence, so the explicit decode costs nothing worth the two unstated
    /// premises.
    pub fn terms_of(&self, entity: u32) -> Option<Vec<u32>> {
        if !self.hasrow.contains(entity) {
            return None;
        }
        let rank = (self.hasrow.rank(entity) - 1) as usize;
        let start = self.offset_at(rank) as usize;
        let end = self.offset_at(rank + 1) as usize;
        // Both bounds were checked monotone and in range at open, so this cannot slice out of the
        // mapping — an `expect` rather than a silent empty, because the alternative is a shorter
        // label set than the entity carries and the write path's 409 not firing.
        Some(
            self.terms[start * 4..end * 4]
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        )
    }

    fn offset_at(&self, index: usize) -> u32 {
        let at = index * 4;
        u32::from_le_bytes([
            self.offsets[at],
            self.offsets[at + 1],
            self.offsets[at + 2],
            self.offsets[at + 3],
        ])
    }

    /// This layer's directory, for a caller assembling an error.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
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
    pub fn terms_of(&self, entity: u32) -> Option<Vec<u32>> {
        self.layers.iter().find_map(|layer| layer.terms_of(entity))
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
            assert_eq!(layer.terms_of(*entity).as_ref(), Some(terms));
        }
        // An entity the layer never held is unknown, and is not the empty list.
        assert_eq!(layer.terms_of(1), None);
        assert_eq!(layer.terms_of(4), None);
        assert_eq!(layer.len(), 4);
    }

    /// **`Some(vec![])` and `None` are different answers**, and conflating them is what would
    /// make the join rule's label arm compare an entity's labels against nothing and pass.
    #[test]
    fn an_empty_list_is_a_value_and_an_absent_entity_is_not() {
        let (_dir, layer) = round_trip(&[(2u32, vec![])]);
        assert_eq!(layer.terms_of(2), Some(Vec::new()));
        assert_eq!(layer.terms_of(3), None);
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
        let stack = EntityTermsStack::open(Some(base.path()), std::slice::from_ref(&paths)).unwrap();
        assert_eq!(stack.terms_of(0), Some(vec![1]));
        assert_eq!(stack.terms_of(1), Some(vec![2, 3]));
        assert_eq!(stack.terms_of(7), Some(vec![4]));
        assert_eq!(stack.terms_of(6), None);
        assert_eq!(stack.layers(), 2);

        // The successor generation's stack, after a flush: the base is shared, not reopened.
        let grown = EntityTermsStack::open(Some(base.path()), &[])
            .unwrap()
            .with_extents(std::slice::from_ref(&paths))
            .unwrap();
        assert_eq!(grown.terms_of(7), Some(vec![4]));
    }

    #[test]
    fn an_empty_stack_answers_nothing_rather_than_failing() {
        assert_eq!(EntityTermsStack::empty().terms_of(0), None);
    }
}
