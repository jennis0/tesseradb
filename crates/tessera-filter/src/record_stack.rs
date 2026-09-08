//! The layered record blob: the build's base plus every flush extent, answering
//! `entity → fields` across all of them (`records-and-search.md` §3, §7).
//!
//! A flush publishes the blob rows of the entities it created as its own extent — the same
//! three-file shape as the base.
//!
//! **The layers are disjoint per column, not per entity** (`ingest.md` §1.4, §6.3). An entity
//! holds a row in the layer that created it, and `POST /control/values` gives it a row in the
//! layer that filled a column on it later, so two layers may hold a row for one entity. What
//! cannot happen is two layers holding the same *column* of one entity: the fill rule leaves one
//! claimant per cell — an absent cell is filled, a cell holding the identical value is a no-op,
//! and a cell holding a different value refuses the batch — so a column has at most one layer
//! that claims it and there is nothing for a merge to arbitrate.
//!
//! A read therefore takes every layer holding a row for the entity and unions their fields, at a
//! cost of **one block decode per column claimant**: one for an entity whose columns were all
//! written by the layer that created it, which is what it paid when this was a first-layer-wins
//! probe, and one more per later fill. Order still decides nothing about correctness; layers are
//! probed base-first because the base holds the overwhelming majority of entities.
//!
//! The wrapper adds no tolerance the single-layer reader lacks: every layer opens through
//! [`RecordBlob::open`]'s fail-closed checks, and an entity in no layer is an ordinary absence
//! (`Ok(None)`), exactly as it is for a base-only bundle.

use std::path::Path;

use crate::record::{RecordBlob, RecordError, RecordField};
use crate::values::Access;

/// One record-blob extent's three files, as the manifest's `record_extents` entry names them.
#[derive(Debug, Clone)]
pub struct RecordExtentPaths {
    pub blocks: std::path::PathBuf,
    pub hasrow: std::path::PathBuf,
    pub directory: std::path::PathBuf,
}

/// The base blob (if the build wrote one) and every flush extent, opened together.
///
/// **Layers are `Arc` so a live generation can be extended without reopening the base.** A flush
/// publishes one more extent; reopening the whole stack for it would remap a base that at 10⁹ is
/// the largest artefact in the bundle, and the successor generation shares every layer the
/// predecessor already had. Appending is sound because a layer is only ever added: an entity id
/// is never reused, and the fill rule keeps one claimant per column, so a layer added later can
/// carry columns of an entity an earlier layer already holds a row for but never the same column.
pub struct RecordStack {
    layers: Vec<std::sync::Arc<RecordBlob>>,
    /// How many rows [`Self::fields_of`] has decoded from a layer, over this stack's life.
    /// Operator- and test-facing: a reader that must answer a column's absence from the schema
    /// and never from a block (`ingest.md` §6.3) is checked against it.
    reads: std::sync::atomic::AtomicU64,
}

impl RecordStack {
    /// Open the stack. `base` is the build's `attrs/record` directory, `None` when the compiled
    /// schema had no blob-resident column at build time; `extents` are the manifest's
    /// `record_extents`, oldest first. Any layer that fails its open refuses the stack — a
    /// missing or malformed layer is a bundle defect, never "those entities have no record"
    /// (records §3's fail-closed rule).
    pub fn open(
        base: Option<&Path>,
        extents: &[RecordExtentPaths],
        access: Access,
    ) -> Result<Self, RecordError> {
        let mut layers = Vec::with_capacity(extents.len() + 1);
        if let Some(dir) = base {
            layers.push(std::sync::Arc::new(RecordBlob::open_dir(dir, access)?));
        }
        for extent in extents {
            layers.push(std::sync::Arc::new(RecordBlob::open(
                &extent.blocks,
                &extent.hasrow,
                &extent.directory,
                access,
            )?));
        }
        Ok(Self {
            layers,
            reads: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// This stack with `extents` appended — the successor generation's, after a flush.
    ///
    /// **A published record extent that no live stack holds answers no drill-down.** The manifest
    /// entry makes the bytes reachable to a *reopen*; the running process serves from the stack it
    /// opened, so a flush that only writes the manifest leaves every entity it flushed with its
    /// blob-resident fields missing — silently, since an entity in no layer is the ordinary
    /// `Ok(None)` — until the next fold or restart. This is the record blob's counterpart to
    /// composing a filter extent onto the live columns, and it is owed at the same moment.
    ///
    /// The existing layers are shared, not reopened.
    pub fn with_extents(
        &self,
        extents: &[RecordExtentPaths],
        access: Access,
    ) -> Result<Self, RecordError> {
        let mut layers = self.layers.clone();
        for extent in extents {
            layers.push(std::sync::Arc::new(RecordBlob::open(
                &extent.blocks,
                &extent.hasrow,
                &extent.directory,
                access,
            )?));
        }
        Ok(Self {
            layers,
            // The count carries across a publication, so a caller watching it over a flush sees
            // one series.
            reads: std::sync::atomic::AtomicU64::new(
                self.reads.load(std::sync::atomic::Ordering::Relaxed),
            ),
        })
    }

    /// How many rows [`Self::fields_of`] and [`Self::for_each_row_in`] have decoded from a layer
    /// since this stack (or the stack it was extended from) was opened. Both read routes count,
    /// one per row decoded, or a caller taking the many-row route would report no blob reads at
    /// all.
    pub fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many layers this stack holds — the base, where the schema had a blob-resident column,
    /// plus one per extent opened onto it. Diagnostic: a caller that wants a *row* asks
    /// [`Self::fields_of`], and this says only how many probes a miss costs.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// The blob-resident fields of `entity`, from every layer that claims one of its columns;
    /// `Ok(None)` when no layer holds a row for it — the ordinary case for an entity all of whose
    /// fields live in the other two homes.
    ///
    /// **One block decode per column claimant** (`ingest.md` §1.4): the layer that created the
    /// entity, plus one per flush that filled a column on it through `POST /control/values`. The
    /// first tag wins where two layers name one column, which the fill rule makes unreachable and
    /// which is here so that a damaged pair answers one value rather than two.
    pub fn fields_of(&self, entity: u32) -> Result<Option<Vec<RecordField>>, RecordError> {
        let mut merged: Option<Vec<RecordField>> = None;
        for layer in &self.layers {
            if !layer.has_row(entity) {
                continue;
            }
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(fields) = layer.fields_of(entity)? else {
                continue;
            };
            match &mut merged {
                None => merged = Some(fields),
                Some(held) => {
                    for field in fields {
                        if !held.iter().any(|f| f.tag == field.tag) {
                            held.push(field);
                        }
                    }
                }
            }
        }
        Ok(merged)
    }

    /// The rows of the entities in `wanted`, from whichever layers hold them — the read a caller
    /// that wants many rows takes instead of looping [`Self::fields_of`], and whose cost is the
    /// blocks touched rather than the entities asked for
    /// ([`RecordBlob::for_each_row_in`] carries the argument).
    ///
    /// Ascending within a layer and layer by layer across the stack, so a caller wanting one
    /// global order must impose it.
    ///
    /// **Each entity is visited exactly once**, which the per-layer walk alone no longer gives:
    /// an entity a values page filled holds a row in two layers (`ingest.md` §1.4), and visiting
    /// it twice would hand the caller two partial rows under one identity. The entities in more
    /// than one layer are taken out of the per-layer walk and answered by [`Self::fields_of`],
    /// which unions their columns; every other entity keeps the block-amortised walk, so the cost
    /// on a stack no page has filled is what it was.
    pub fn for_each_row_in(
        &self,
        wanted: &croaring::Bitmap,
        f: &mut dyn FnMut(u32, Vec<RecordField>) -> Result<(), RecordError>,
    ) -> Result<(), RecordError> {
        let mut seen = croaring::Bitmap::new();
        let mut shared = croaring::Bitmap::new();
        for layer in &self.layers {
            let mut here = layer.hasrow().clone();
            here.and_inplace(wanted);
            let mut again = here.clone();
            again.and_inplace(&seen);
            shared.or_inplace(&again);
            seen.or_inplace(&here);
        }
        if shared.is_empty() {
            for layer in &self.layers {
                layer.for_each_row_in(wanted, &mut |entity, fields| {
                    self.reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    f(entity, fields)
                })?;
            }
            return Ok(());
        }
        let mut alone = wanted.clone();
        alone.andnot_inplace(&shared);
        for layer in &self.layers {
            layer.for_each_row_in(&alone, &mut |entity, fields| {
                self.reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                f(entity, fields)
            })?;
        }
        for entity in shared.iter() {
            if let Some(fields) = self.fields_of(entity)? {
                f(entity, fields)?;
            }
        }
        Ok(())
    }

    /// Whether any layer holds a row for `entity`.
    pub fn has_row(&self, entity: u32) -> bool {
        self.layers.iter().any(|layer| layer.has_row(entity))
    }

    /// Every layer's own addressing self-check, for the conformance surface.
    pub fn self_check(&self) -> Result<(), RecordError> {
        for layer in &self.layers {
            layer.self_check()?;
        }
        Ok(())
    }
}
