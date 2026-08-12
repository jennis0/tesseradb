//! The layered record blob: the build's base plus every flush extent, answering
//! `entity → fields` across all of them (`records-and-search.md` §3, §7).
//!
//! A flush publishes the blob rows of the entities it created as its own extent — the same
//! three-file shape as the base — and the layers are **disjoint in entity space** (I9: an
//! entity is allocated once and its row is written by exactly one layer). Disjointness is what
//! makes this wrapper five lines of logic rather than a merge: the layer whose has-row bitmap
//! contains the entity answers, and no other layer can contradict it. Order therefore decides
//! nothing about correctness; layers are probed base-first only because the base holds the
//! overwhelming majority of entities.
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
pub struct RecordStack {
    layers: Vec<RecordBlob>,
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
            layers.push(RecordBlob::open_dir(dir, access)?);
        }
        for extent in extents {
            layers.push(RecordBlob::open(
                &extent.blocks,
                &extent.hasrow,
                &extent.directory,
                access,
            )?);
        }
        Ok(Self { layers })
    }

    /// The blob-resident fields of `entity`, from whichever layer holds its row; `Ok(None)` when
    /// no layer does — the ordinary case for an entity all of whose fields live in the other two
    /// homes.
    pub fn fields_of(&self, entity: u32) -> Result<Option<Vec<RecordField>>, RecordError> {
        for layer in &self.layers {
            if layer.has_row(entity) {
                return layer.fields_of(entity);
            }
        }
        Ok(None)
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
