//! **What a spatial level holds, and when it is built** (`polygon-membership.md` §6.3).
//!
//! `membership = "spatial"` is *the rows whose stored position is inside the shape*, exactly, for
//! every kind. Nothing about that membership is stored in an artifact's record — the record holds
//! the shape — so it has to be resolved against the rows, and this module decides where and when.
//! Three things are held, at three lifetimes:
//!
//! - **Per artifact, for the artifact's life**: the canonical shape and its decomposition — the
//!   interior tiles whole, the boundary cells as code and parity only
//!   ([`tessera_store::derived::HeldShape`]). Built from the record blobs at open and at every
//!   publication into the level; never on a request.
//! - **Per level**: a coarse geometry-derived index from tiles to the artifacts whose bounds meet
//!   them ([`tessera_store::derived::ShapeIndex`]), so a segment is tested only against the
//!   shapes it can touch.
//! - **Per segment**: the membership of every row in it, resolved when the segment is published —
//!   in the flush's own unit of work on the pool, before the generation swaps; at the fold's
//!   artifact pass for the segments the fold wrote; at open for every segment the bundle holds.
//!   A segment is immutable and its id is never reused (contracts §2.1), so a piece keyed by
//!   `seg_id` is valid for as long as the segment is served, across every generation that carries
//!   it. A row is tested once in its life.
//!
//! **The base segment's piece is persisted and claimed, never resolved twice across a restart.**
//! The build and every fold write what they resolved in the layout's own form — the row-major
//! column, or the `shape-rows` row form where the level is artifact-major
//! (`tessera_store::derived::file_shape_rows`) — keyed by the segment id, the segment's row
//! count and the level version. [`ShapeStore::warm`] claims those files at open through
//! [`PersistedPieces`], refuses one whose key does not equal what it is resolving for (I11: never
//! adapted), and resolves only the segments no file covers — the flushed ones, which at steady
//! state are zero or a few small pieces. A refused file is said so at `warn`; a segment with no
//! file at all is the ordinary case for a flush segment and is said so at `info`.
//!
//! **The decompositions are persisted too**, per artifact per view in one `shape-held` file per
//! level (`tessera_store::derived::shape_held_bytes`), because on Overture's part 0 the descent
//! was 8.9 s of a 9.3 s open once the pieces were claimed and the decode alone is 76 ms. An entry
//! is used only for the canonical bytes it was descended from — length and digest — and under the
//! level version it was written at; otherwise the shape is decomposed again, counted, and said.
//!
//! **Per generation the pieces are joined with the row bases applied** ([`ShapeLevel::joined`]) —
//! an `add_offset` and a union per segment, O(containers) — into the per-row source the serving
//! layout machinery consumes as it consumes an enumerated layer's member table: the row form, the
//! tile index, the row-major column, the masked-count histogram
//! (`crate::artifacts::ArtifactProjections::get_or_build`). Nothing downstream of the join knows
//! the membership came from a shape.
//!
//! **The fallback, and why it is loud.** Every publication that introduces a segment resolves it
//! before the swap, so a request should never find a piece missing. If one is missing — a
//! publication route this module was not wired into — [`ShapeLevel::joined`] resolves it on the
//! request path and warns, rather than serving the level with the segment's rows absent: absent
//! rows would be a masked count that silently understates for every viewer, which is the failure
//! the design's conformance section names as invisible from inside. Correctness rests on the
//! fallback; the cost budget rests on the hooks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use croaring::Bitmap;
use tessera_lifecycle::membership::ArtifactStore;
use tessera_store::derived::{resolve_segment, HeldEntry, HeldShape, ShapeIndex};
use tessera_store::manifest::{RowColumnExtent, ShapeHeldExtent, ShapeRowsExtent};
use tessera_store::read::{Bundle, SegmentData, ViewData};
use tessera_types::layer::{MembershipSource, RegisteredLayer};

use crate::row_column::RowColumn;

// The publication-side vocabulary, re-exported for the server, which sees engine API types only
// (`scripts/check-layers.sh`): what a row's shape is read into, how it is canonicalised, and what
// that reported.
pub use tessera_spatial::shape::{CanonError, CanonReport, Shape, ShapeF64, Space};
pub use tessera_spatial::{Bounds, Projection};
pub use tessera_store::derived::{
    authored_shape_input, canonical_shapes, shape_input, CanonicalShapes, ShapeInput,
    ShapeRefusal, ShapeSpace, ShapeStats,
};
pub use tessera_types::layer::DrawnShape;

/// The per-artifact vertex budget a served shape is guarded by — the hull's 2,048, for the
/// hull's reason (`artifact-shapes.md` §8 B): a guard on the wire, not a control on the shape.
pub const SERVED_VERTEX_BUDGET: usize = 2_048;

/// The vertex rule's tolerance at a request depth, in grid units (`polygon-membership.md` §7.2):
/// the side of the cell a screen pixel covers at that zoom. A depth-`z` tile is 512 pixels wide
/// on the client (`clients/ts/core/src/coords.ts`, measured), so a pixel is the depth-`z + 9`
/// cell — `2^(32 − z − 9)` grid units on the 32-bit-per-axis grid — and a vertex that would move
/// the drawn edge by less than that is not sent. `None` — the identifier route, which carries no
/// depth — is the finest cell, so the whole presimplified shape under the budget alone.
pub fn served_tolerance(zoom: Option<u8>) -> u32 {
    match zoom {
        Some(z) => 1u32 << (32u32.saturating_sub(u32::from(z) + 9)).min(31),
        None => 1,
    }
}

/// A held shape as the wire carries it — parts, rings, vertices in grid units — at a request's
/// depth, under the vertex budget, and whether the budget cut what the depth alone would have
/// kept (§7.2). One function for the predicate and the authored kind, so the two cannot be
/// simplified differently; the derived kind is the hull and is digested at derivation.
pub fn served_rings(
    shape: &tessera_spatial::shape::Shape,
    zoom: Option<u8>,
) -> (Vec<Vec<Vec<[u32; 2]>>>, bool) {
    let (parts, guarded) = shape.rings_guarded(served_tolerance(zoom), SERVED_VERTEX_BUDGET);
    let parts = parts
        .into_iter()
        .map(|rings| {
            rings
                .into_iter()
                .map(|ring| ring.into_iter().map(|(x, y)| [x, y]).collect())
                .collect()
        })
        .collect();
    (parts, guarded)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One `(view, layer, level)`'s held shapes, their index, and the per-segment pieces resolved
/// against them.
pub struct ShapeLevel {
    pub view: String,
    pub layer: String,
    pub level: u32,
    /// The level version the shapes were read at. A publication into the level moves it, and
    /// [`ShapeStore::level`] rebuilds on a mismatch.
    pub level_version: u64,
    /// Parallel to the level's ordinals; `None` for a hole and for a record whose bytes would not
    /// decode as a shape — counted in [`ShapeLevel::undecodable`] and alarmed at build, because
    /// such an artifact holds nothing for every viewer.
    pub shapes: Vec<Option<HeldShape>>,
    pub index: ShapeIndex,
    pub undecodable: u64,
    /// Wall time of the build — decoding and decomposing every shape — and its two halves, so
    /// the open-time trace says which of them a persisted decomposition would remove.
    pub build_ms: u64,
    pub decode_ms: u64,
    pub decompose_ms: u64,
    /// Decompositions taken from the prefix's persisted file, and those descended here because
    /// no persisted entry matched the shape.
    pub held_claimed: u64,
    pub held_decomposed: u64,
    pieces: Mutex<HashMap<String, Arc<Vec<Option<Bitmap>>>>>,
}

/// One segment's resolution, for the traces and the build report.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolutionCost {
    pub rows_tested: u64,
    pub rows_interior: u64,
    pub artifacts_skipped: u64,
    /// Artifacts holding a shape and no row of the segment.
    pub artifacts_empty: u64,
    pub elapsed_ms: u64,
}

impl ShapeLevel {
    /// Build from the store's record blobs: every shape of `(layer, level)` canonicalised for
    /// `view`, decoded and decomposed — or, where `persisted` holds an entry descended from the
    /// same canonical bytes, assembled from it.
    pub fn build(
        view: &str,
        layer: &str,
        level: u32,
        store: &ArtifactStore,
        mut persisted: Vec<Option<HeldEntry>>,
    ) -> ShapeLevel {
        let started = Instant::now();
        let mut held_claimed = 0u64;
        let mut held_decomposed = 0u64;
        let ordinals = store
            .level(layer, level)
            .map(|(o, _)| o as usize + 1)
            .max()
            .unwrap_or(0);
        let mut shapes: Vec<Option<HeldShape>> = Vec::with_capacity(ordinals);
        let mut undecodable = 0u64;
        let mut decode_ns = 0u128;
        let mut decompose_ns = 0u128;
        for ordinal in 0..ordinals as u32 {
            let held = store
                .shape_of(layer, level, ordinal)
                .and_then(|shapes| shapes.for_view(view))
                .and_then(|bytes| {
                    let at = Instant::now();
                    let decoded = tessera_spatial::shape::Shape::decode(bytes);
                    decode_ns += at.elapsed().as_nanos();
                    match decoded {
                    Ok(shape) => {
                        let at = Instant::now();
                        let entry = persisted
                            .get_mut(ordinal as usize)
                            .and_then(Option::take)
                            .filter(|entry| entry.is_of(bytes));
                        let held = match entry {
                            Some(entry) => {
                                held_claimed += 1;
                                entry.into_held(shape)
                            }
                            None => {
                                held_decomposed += 1;
                                HeldShape::new(shape)
                            }
                        };
                        decompose_ns += at.elapsed().as_nanos();
                        Some(held)
                    }
                    Err(error) => {
                        undecodable += 1;
                        tracing::error!(
                            layer = %layer,
                            level,
                            view = %view,
                            ordinal,
                            %error,
                            "ALARM: an artifact's stored shape would not decode; it holds no rows \
                             for any viewer until it is republished"
                        );
                        None
                    }
                    }
                });
            shapes.push(held);
        }
        let index = ShapeIndex::build(&shapes);
        ShapeLevel {
            view: view.to_string(),
            layer: layer.to_string(),
            level,
            level_version: store.level_version(layer, level),
            shapes,
            index,
            undecodable,
            build_ms: started.elapsed().as_millis() as u64,
            decode_ms: (decode_ns / 1_000_000) as u64,
            decompose_ms: (decompose_ns / 1_000_000) as u64,
            held_claimed,
            held_decomposed,
            pieces: Mutex::new(HashMap::new()),
        }
    }

    /// How many artifacts hold a shape.
    pub fn artifacts(&self) -> u64 {
        self.shapes.iter().filter(|s| s.is_some()).count() as u64
    }

    /// The decomposition's size over the level: interior tiles and boundary cells, total and the
    /// maximum any one artifact holds.
    pub fn decomposition_size(&self) -> (u64, u64, u64, u64) {
        let mut interior = 0u64;
        let mut boundary = 0u64;
        let mut max_interior = 0u64;
        let mut max_boundary = 0u64;
        for held in self.shapes.iter().flatten() {
            let i = held.interior.len() as u64;
            let b = held.boundary.len() as u64;
            interior += i;
            boundary += b;
            max_interior = max_interior.max(i);
            max_boundary = max_boundary.max(b);
        }
        (interior, boundary, max_interior, max_boundary)
    }

    /// Bytes held for the decompositions, beyond the shapes' own.
    pub fn held_bytes(&self) -> u64 {
        self.shapes
            .iter()
            .flatten()
            .map(HeldShape::held_bytes)
            .sum()
    }

    /// Resolve one segment and hold the piece under its id, replacing any earlier piece for the
    /// same id (a re-resolution after a shape republication).
    pub fn resolve(&self, segment: &SegmentData) -> (Arc<Vec<Option<Bitmap>>>, ResolutionCost) {
        let started = Instant::now();
        let resolved = resolve_segment(segment, &self.shapes, &self.index);
        let cost = ResolutionCost {
            rows_tested: resolved.rows_tested,
            rows_interior: resolved.rows_interior,
            artifacts_skipped: resolved.artifacts_skipped,
            artifacts_empty: resolved.artifacts_empty,
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        let piece = Arc::new(resolved.rows);
        lock(&self.pieces).insert(segment.seg_id.clone(), Arc::clone(&piece));
        (piece, cost)
    }

    /// Install a piece resolved elsewhere — the flush's, resolved on the pool before publication.
    pub fn install(&self, seg_id: &str, piece: Arc<Vec<Option<Bitmap>>>) {
        lock(&self.pieces).insert(seg_id.to_string(), piece);
    }

    pub fn piece(&self, seg_id: &str) -> Option<Arc<Vec<Option<Bitmap>>>> {
        lock(&self.pieces).get(seg_id).cloned()
    }

    pub fn has_piece(&self, seg_id: &str) -> bool {
        lock(&self.pieces).contains_key(seg_id)
    }

    /// Every segment's piece, with the row bases applied and unioned: the level's membership in
    /// this generation's view row space, one bitmap per ordinal.
    ///
    /// Resolves a missing piece on the spot and says so — see the module doc on why the fallback
    /// is loud rather than absent.
    pub fn joined(&self, segments: &[(&SegmentData, u32)]) -> Vec<Option<Bitmap>> {
        let mut rows: Vec<Option<Bitmap>> = self
            .shapes
            .iter()
            .map(|held| held.as_ref().map(|_| Bitmap::new()))
            .collect();
        for (segment, row_base) in segments {
            let piece = match self.piece(&segment.seg_id) {
                Some(piece) => piece,
                None => {
                    tracing::warn!(
                        layer = %self.layer,
                        level = self.level,
                        view = %self.view,
                        seg_id = %segment.seg_id,
                        "a segment reached a request unresolved against this level's shapes; \
                         resolved now, on the request path — every publication route should have \
                         resolved it before the swap"
                    );
                    self.resolve(segment).0
                }
            };
            for (ordinal, part) in piece.iter().enumerate() {
                if let (Some(part), Some(rows)) = (part, rows.get_mut(ordinal).and_then(Option::as_mut)) {
                    if !part.is_empty() {
                        rows.or_inplace(&part.add_offset(i64::from(*row_base)));
                    }
                }
            }
        }
        for rows in rows.iter_mut().flatten() {
            rows.run_optimize();
        }
        rows
    }

    /// Drop the pieces of segments no generation serves any more.
    pub fn retain_segments(&self, live: &dyn Fn(&str) -> bool) {
        lock(&self.pieces).retain(|seg_id, _| live(seg_id));
    }

    pub fn pieces_held(&self) -> usize {
        lock(&self.pieces).len()
    }
}

/// Every spatial level's held structures, keyed by `(view, layer, level)`.
#[derive(Default)]
pub struct ShapeStore {
    levels: Mutex<HashMap<(String, String, u32), Arc<ShapeLevel>>>,
    /// What the last [`ShapeStore::warm`] did — the open's, until a publication runs another.
    last_warm: Mutex<WarmReport>,
}

impl ShapeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, view: &str, layer: &str, level: u32) -> Option<Arc<ShapeLevel>> {
        lock(&self.levels)
            .get(&(view.to_string(), layer.to_string(), level))
            .cloned()
    }

    /// The level's held structures at the store's current level version, built if absent or
    /// stale. The build runs outside the lock — it is the expensive step — and a concurrent build
    /// of the same level at the same version is harmless, the two being equal.
    pub fn level(
        &self,
        view: &str,
        layer: &str,
        level: u32,
        store: &ArtifactStore,
        persisted: &PersistedPieces<'_>,
    ) -> Arc<ShapeLevel> {
        let version = store.level_version(layer, level);
        if let Some(held) = self.get(view, layer, level) {
            if held.level_version == version {
                return held;
            }
        }
        let entries = persisted.held_entries(view, layer, level, version);
        let built = Arc::new(ShapeLevel::build(view, layer, level, store, entries));
        let (interior, boundary, max_interior, max_boundary) = built.decomposition_size();
        tracing::info!(
            layer = %layer,
            level,
            view = %view,
            artifacts = built.artifacts(),
            undecodable = built.undecodable,
            interior_tiles = interior,
            boundary_cells = boundary,
            max_interior_tiles = max_interior,
            max_boundary_cells = max_boundary,
            held_bytes = built.held_bytes(),
            index_entries = built.index.entries(),
            build_ms = built.build_ms,
            decode_ms = built.decode_ms,
            decompose_ms = built.decompose_ms,
            held_claimed = built.held_claimed,
            held_decomposed = built.held_decomposed,
            "a spatial level's shapes were decoded, and decomposed or claimed from the prefix"
        );
        lock(&self.levels).insert(
            (view.to_string(), layer.to_string(), level),
            Arc::clone(&built),
        );
        built
    }

    /// Every held level of one view.
    pub fn levels_of_view(&self, view: &str) -> Vec<Arc<ShapeLevel>> {
        lock(&self.levels)
            .iter()
            .filter(|((v, _, _), _)| v == view)
            .map(|(_, held)| Arc::clone(held))
            .collect()
    }

    pub fn forget_layer(&self, layer: &str) {
        lock(&self.levels).retain(|(_, l, _), _| l != layer);
    }

    /// Drop every piece for a segment no view of the generation serves — after a merge or a fold
    /// has replaced the segments it read.
    pub fn retain_segments(&self, live: &dyn Fn(&str) -> bool) {
        for held in lock(&self.levels).values() {
            held.retain_segments(live);
        }
    }

    pub fn held(&self) -> usize {
        lock(&self.levels).len()
    }

    /// Claim or resolve every segment of `view_data` that `level` does not yet hold a piece for.
    ///
    /// What open, a publication into the level and a fold all do: the segments are the
    /// generation's, the shapes are the level's, and a piece is keyed by a segment id that is
    /// never reused — so a segment already resolved against this level version is never
    /// resolved twice. A segment `persisted` names is read rather than resolved, where the file's
    /// key equals this level version and this segment; otherwise it is resolved and the reason is
    /// said.
    pub fn resolve_missing(
        level: &ShapeLevel,
        view_data: &ViewData,
        persisted: &PersistedPieces<'_>,
        store: &ArtifactStore,
    ) -> WarmReport {
        let mut total = WarmReport::default();
        for segment in &view_data.segments {
            if level.has_piece(&segment.seg_id) {
                continue;
            }
            let started = Instant::now();
            if let Some(piece) = persisted.claim(level, segment, store) {
                level.install(&segment.seg_id, Arc::new(piece));
                total.pieces_claimed += 1;
                total.claim_ms += started.elapsed().as_millis() as u64;
                continue;
            }
            let (_, cost) = level.resolve(segment);
            total.pieces_resolved += 1;
            total.rows_tested += cost.rows_tested;
            total.rows_interior += cost.rows_interior;
            total.resolve_ms += cost.elapsed_ms;
        }
        total
    }

    /// Build every spatial level's held structures and resolve every segment the bundle serves —
    /// what `Engine::open` does before it serves anything, and what a publication into a layer
    /// does for that layer (`polygon-membership.md` §6.3: built at publication and at open, never
    /// on a request).
    ///
    /// `only` narrows the pass to one layer where a publication moved just that one; `persisted`
    /// is what the prefix holds already resolved, claimed before anything is resolved.
    pub fn warm(
        &self,
        bundle: &Bundle,
        layers: &[RegisteredLayer],
        store: &ArtifactStore,
        only: Option<&str>,
        persisted: &PersistedPieces<'_>,
    ) -> WarmReport {
        let started = Instant::now();
        let mut report = WarmReport::default();
        for registered in layers {
            let declaration = &registered.declaration;
            if declaration.membership != MembershipSource::Spatial || declaration.shape.is_none() {
                continue;
            }
            if only.is_some_and(|name| name != declaration.name) {
                continue;
            }
            for view in &declaration.views {
                let Some(view_data) = bundle
                    .partitions
                    .values()
                    .find_map(|partition| partition.views.get(view))
                else {
                    continue;
                };
                for level in 0..registered.runs.len() as u32 {
                    let held = self.level(view, &declaration.name, level, store, persisted);
                    let cost = Self::resolve_missing(&held, view_data, persisted, store);
                    report.levels += 1;
                    report.artifacts += held.artifacts();
                    report.rows_tested += cost.rows_tested;
                    report.rows_interior += cost.rows_interior;
                    report.resolve_ms += cost.resolve_ms;
                    report.pieces_claimed += cost.pieces_claimed;
                    report.pieces_resolved += cost.pieces_resolved;
                    report.claim_ms += cost.claim_ms;
                    report.build_ms += held.build_ms;
                    report.held_claimed += held.held_claimed;
                    report.held_decomposed += held.held_decomposed;
                    report.held_bytes += held.held_bytes();
                }
            }
        }
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        *lock(&self.last_warm) = report;
        report
    }

    /// What the last warm pass did — the open's, for the operator plane and the tests that assert
    /// an open claimed rather than resolved.
    pub fn last_warm(&self) -> WarmReport {
        *lock(&self.last_warm)
    }
}

/// What a warm pass did, for the open-time trace and the reports.
#[derive(Debug, Clone, Copy, Default)]
pub struct WarmReport {
    pub levels: u64,
    pub artifacts: u64,
    pub rows_tested: u64,
    pub rows_interior: u64,
    pub build_ms: u64,
    pub resolve_ms: u64,
    pub held_bytes: u64,
    pub elapsed_ms: u64,
    /// Segment pieces read from the prefix's persisted forms rather than resolved.
    pub pieces_claimed: u64,
    /// Segment pieces resolved from the geometry — the segments no persisted form covered.
    pub pieces_resolved: u64,
    pub claim_ms: u64,
    /// Decompositions claimed from the prefix's persisted files, and those descended at this pass.
    pub held_claimed: u64,
    pub held_decomposed: u64,
}

/// The prefix's persisted, already-resolved shape memberships, as an open claims them.
///
/// Two forms, both written by the build and by every fold and both keyed by the level version:
/// the `shape-rows` row form, which also names its segment and the segment's row count; and the
/// row-major column, which is over the view's base rows and so is the base segment's piece —
/// inverted here into the same per-ordinal bitmaps. **Equality on every key, never anything
/// weaker** (I11): a piece that does not match is resolved again from the geometry, and the
/// refusal is said.
#[derive(Clone, Copy, Default)]
pub struct PersistedPieces<'a> {
    pub prefix_dir: Option<&'a std::path::Path>,
    pub shape_rows: &'a [ShapeRowsExtent],
    pub row_columns: &'a [RowColumnExtent],
    pub shape_held: &'a [ShapeHeldExtent],
}

impl PersistedPieces<'_> {
    /// Nothing to claim — what a publication into a layer passes, its level version having just
    /// moved past every file the prefix holds.
    pub fn none() -> Self {
        Self::default()
    }

    /// The persisted decompositions of one level at `version`, or nothing — with the refusal
    /// said — where the prefix holds none, holds one for another version, or holds one that will
    /// not read.
    fn held_entries(&self, view: &str, layer: &str, level: u32, version: u64) -> Vec<Option<HeldEntry>> {
        let Some(prefix_dir) = self.prefix_dir else {
            return Vec::new();
        };
        let Some(extent) = self
            .shape_held
            .iter()
            .find(|e| e.view == view && e.layer == layer && e.level == level)
        else {
            return Vec::new();
        };
        if extent.level_version != version {
            tracing::warn!(
                layer = %layer,
                level,
                view = %view,
                written_at = extent.level_version,
                now = version,
                "the prefix's persisted decompositions are not this level version's; every shape \
                 is decomposed again"
            );
            return Vec::new();
        }
        match tessera_store::derived::read_shape_held(&prefix_dir.join(&extent.path), version) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    layer = %layer,
                    level,
                    view = %view,
                    path = %extent.path,
                    %error,
                    "the prefix's persisted decompositions would not be read; every shape is \
                     decomposed again"
                );
                Vec::new()
            }
        }
    }

    /// The piece for `segment` under `level`, if a persisted form supplies it.
    fn claim(
        &self,
        level: &ShapeLevel,
        segment: &SegmentData,
        store: &ArtifactStore,
    ) -> Option<Vec<Option<Bitmap>>> {
        let prefix_dir = self.prefix_dir?;
        let same_level = |view: &str, layer: &str, lvl: u32| {
            view == level.view && layer == level.layer && lvl == level.level
        };
        if let Some(extent) = self
            .shape_rows
            .iter()
            .find(|e| same_level(&e.view, &e.layer, e.level) && e.seg_id == segment.seg_id)
        {
            if extent.level_version != level.level_version || extent.row_count != segment.row_count {
                tracing::warn!(
                    layer = %level.layer,
                    level = level.level,
                    view = %level.view,
                    seg_id = %segment.seg_id,
                    written_at = extent.level_version,
                    now = level.level_version,
                    written_rows = extent.row_count,
                    rows = segment.row_count,
                    "a persisted shape row form is refused: its key is not this level version \
                     and this segment; the segment is resolved again from the geometry"
                );
                return None;
            }
            match tessera_store::derived::read_shape_rows(
                &prefix_dir.join(&extent.path),
                level.level_version,
                &segment.seg_id,
                segment.row_count,
            ) {
                Ok(rows) if rows.len() == level.shapes.len() => return Some(rows),
                Ok(rows) => tracing::warn!(
                    layer = %level.layer,
                    level = level.level,
                    view = %level.view,
                    seg_id = %segment.seg_id,
                    file_ordinals = rows.len(),
                    level_ordinals = level.shapes.len(),
                    "a persisted shape row form is refused: it covers a different number of \
                     ordinals than the level holds; the segment is resolved again"
                ),
                Err(error) => tracing::warn!(
                    layer = %level.layer,
                    level = level.level,
                    view = %level.view,
                    seg_id = %segment.seg_id,
                    path = %extent.path,
                    %error,
                    "a persisted shape row form named by the manifest would not be read; the \
                     segment is resolved again from the geometry"
                ),
            }
            return None;
        }
        // The column is the base's piece: it is addressed in the view's base row space, which is
        // exactly one segment at row base zero. A segment whose row count is not the column's is
        // not that segment.
        if let Some(extent) = self
            .row_columns
            .iter()
            .find(|e| same_level(&e.view, &e.layer, e.level))
        {
            if extent.level_version != level.level_version {
                tracing::warn!(
                    layer = %level.layer,
                    level = level.level,
                    view = %level.view,
                    written_at = extent.level_version,
                    now = level.level_version,
                    "a persisted row-major column is not this level version's; the base segment \
                     is resolved again from the geometry"
                );
                return None;
            }
            let column = match RowColumn::open(&prefix_dir.join(&extent.path), extent.layout) {
                Ok(column) => column,
                Err(error) => {
                    tracing::warn!(
                        layer = %level.layer,
                        level = level.level,
                        view = %level.view,
                        path = %extent.path,
                        %error,
                        "a persisted row-major column would not open; the base segment is \
                         resolved again from the geometry"
                    );
                    return None;
                }
            };
            if column.base_rows() != segment.row_count {
                // Not the base segment — a flushed one, which no column covers. Ordinary.
                tracing::info!(
                    layer = %level.layer,
                    level = level.level,
                    view = %level.view,
                    seg_id = %segment.seg_id,
                    "no persisted form covers this segment; it is resolved from the geometry"
                );
                return None;
            }
            if column.len() != level.shapes.len() {
                tracing::warn!(
                    layer = %level.layer,
                    level = level.level,
                    view = %level.view,
                    column_ordinals = column.len(),
                    level_ordinals = level.shapes.len(),
                    "a persisted row-major column covers a different number of ordinals than \
                     the level holds; the base segment is resolved again"
                );
                return None;
            }
            return Some(invert_column(&column, level, store));
        }
        tracing::info!(
            layer = %level.layer,
            level = level.level,
            view = %level.view,
            seg_id = %segment.seg_id,
            "no persisted form covers this segment; it is resolved from the geometry"
        );
        None
    }
}

/// A row-major column read back as per-ordinal bitmaps of the base segment's rows.
///
/// A hole is an ordinal the level holds no artifact at (`store.level` yields no record); an
/// artifact the column labels no row with is an empty membership, which is a different fact.
fn invert_column(column: &RowColumn, level: &ShapeLevel, store: &ArtifactStore) -> Vec<Option<Bitmap>> {
    let mut rows: Vec<Option<Bitmap>> = vec![None; level.shapes.len()];
    for (ordinal, _) in store.level(&level.layer, level.level) {
        if let Some(slot) = rows.get_mut(ordinal as usize) {
            *slot = Some(Bitmap::new());
        }
    }
    for row in 0..column.base_rows() {
        column.for_each_label(row, |ordinal| {
            if let Some(Some(rows)) = rows.get_mut(ordinal as usize) {
                rows.add(row);
            }
        });
    }
    for rows in rows.iter_mut().flatten() {
        rows.run_optimize();
    }
    rows
}

/// One flush segment's resolution against one level, carried from the pool to the publication
/// (`crate::flush`): the level it was resolved against, so a publication that finds the level
/// rebuilt meanwhile re-resolves against the current one rather than installing a piece computed
/// over the shapes of a level version no longer served.
pub struct ShapePiece {
    pub level: Arc<ShapeLevel>,
    pub rows: Arc<Vec<Option<Bitmap>>>,
    pub cost: ResolutionCost,
}
