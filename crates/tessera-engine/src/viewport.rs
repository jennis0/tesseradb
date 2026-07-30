//! The masked viewport query (task-11 brief, design §2.6 retrieve steps 1–9).
//!
//! [`Engine::viewport`] loads the generation pointer exactly once, validates or mints the pin
//! (I11: geometry identity only, `(prefix, segments_version)` — never `overlay_version`, so an
//! overlay swap never invalidates an outstanding pin), gets-or-builds the session's cached row
//! projection, composes the effective mask (I1), and for every tile touching `bbox` counts and
//! samples. The sampler is a **deliberate placeholder** (I7) — see [`sample_tile`]'s doc — not
//! the real priority-sample definition.

use std::ops::Range;
use std::sync::Arc;

use tessera_spatial::{tiles_for_bbox, Extent};
use tessera_store::manifest::{DeclaredScalar, Quantisation};
use tessera_store::read::{ScalarSlice, SegmentData};
use tessera_store::tile_ranges;
use tessera_store::StoreError;
use tessera_types::{EntityId, PinId, TesseraId, API_VERSION};

use crate::compose::{compose, visible_to, EffectiveMask, RowProjection};
use crate::session::{Engine, EngineError, Result, Session};
use crate::Generation;

/// One declared-scalar value carried alongside a point (mirrors `tessera_spatial::ScalarValue`'s
/// three Phase 1 kinds, but on the *output* side — read from `ColumnsRef`, not staged for write).
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarOut {
    U64(u64),
    F32(f32),
    Utf8(String),
}

/// One tile's count row: `matched == visible` always in Phase 1 (no filters yet — Reference
/// Sheet R5).
#[derive(Debug, Clone, PartialEq)]
pub struct TileCount {
    /// The tile's Morton prefix at the request's zoom depth.
    pub tile: u64,
    pub visible: u64,
    pub matched: u64,
}

/// One sampled point.
///
/// **I10, strengthened (contracts r6):** no entity ID leaves the engine on this path, because
/// none is stored. `columns.arrow` carries `tessera_id` at the row, so the gather reads the
/// identity it is allowed to show and cannot read the one it is not. Entity IDs survive only in
/// entity-space structures and as `permutation.bin`'s index — never as a value on any path
/// reaching `tessera-wire`.
#[derive(Debug, Clone, PartialEq)]
pub struct PointOut {
    pub tessera_id: TesseraId,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<ScalarOut>,
}

/// The masked viewport response. No `serde` derive (I10) — see [`PointOut`]'s doc.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewportOut {
    pub pin: PinId,
    pub tiles: Vec<TileCount>,
    pub points: Vec<PointOut>,
}

/// `POST /v1/items/{handle}`'s payload (R5): a visible item's scalars plus its caller-supplied
/// external id, if it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOut {
    pub scalars: Vec<ScalarOut>,
    pub external_id: Option<Vec<u8>>,
}

/// `GET /v1/meta`'s payload (R5) — the bundle-level facts a viewer client needs before it can
/// issue a sensible `/v1/viewport` call.
#[derive(Debug, Clone)]
pub struct EngineMeta {
    pub api_version: u32,
    pub bundle_format: u32,
    /// `(id, display_name)` pairs, in manifest order.
    pub slices: Vec<(String, String)>,
    pub quantisation: Quantisation,
    pub declared_scalars: Vec<DeclaredScalar>,
}

impl Engine {
    /// `GET /v1/meta` (R5): read-only bundle facts, no session/authorisation involved. Loads the
    /// generation once, like every other request path.
    pub fn meta(&self) -> EngineMeta {
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        EngineMeta {
            api_version: API_VERSION,
            bundle_format: manifest.bundle_format,
            slices: manifest
                .slices
                .iter()
                .map(|s| (s.id.clone(), s.display_name.clone()))
                .collect(),
            quantisation: manifest.quantisation,
            declared_scalars: manifest.declared_scalars.clone(),
        }
    }

    /// Is `entity` visible to `session` under `generation` — the ONE BIT `/v1/items` needs. An
    /// **entity-space** question (see `crate::compose::visible_to`'s doc): three constant-time
    /// probes, no `RowProjection` constructed or consulted, so this costs the same whether
    /// `entity` exists and is visible, exists and is not, or does not exist at all (Critical
    /// C-5, closed rather than narrowed).
    pub fn visible_to(&self, session: &Session, generation: &Generation, entity: EntityId) -> bool {
        visible_to(
            &session.fragment,
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            entity,
        )
    }

    /// `POST /v1/items/{handle}` (R5): invert `id` to its entity, test visibility in entity
    /// space, and only then locate a row and read its scalars/external id.
    ///
    /// Returns `Ok(None)` both when `id` names nothing in this bundle and when it names an item
    /// the principal may not see — deliberately one outcome from one code path, so the server
    /// cannot differentiate what the engine does not tell it (owner ruling; contracts §3.2).
    ///
    /// **The timing channel is closed, not narrowed** (Critical C-5; design Appendix C, C4
    /// annotation). Inversion is a pure function taking no I/O. The visibility test that follows
    /// is an entity-space question — three constant-time probes — and is **the same three probes
    /// for an identifier that names nothing and one that names an invisible item**. No
    /// `RowProjection` is constructed or read, so there is no per-ID cost for an attacker to
    /// correlate against, warm or cold. A row is located only after the answer is already
    /// "visible", and the sidecar is read only after that.
    ///
    /// **Returns `Err` rather than a fail-open `None`** (Critical N-3). A digest mismatch, an
    /// out-of-order extent or a short locator is a `500`, never an item served with
    /// `external_id: null` — `.ok().flatten()` would discard exactly the typed errors Task 8
    /// exists to produce. This does not reopen C-5: the sidecar is touched only for an item
    /// already established as visible, so no attacker-drivable path can raise it.
    pub fn item(
        &self,
        session: &Session,
        id: TesseraId,
    ) -> std::result::Result<Option<ItemOut>, StoreError> {
        let generation = self.generation.load_full();
        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // ONE BIT, in entity space, O(1), before anything is looked up in row space.
        if !self.visible_to(session, &generation, entity) {
            return Ok(None);
        }

        // Visible. Now — and only now — find the row, so the cost below is never reachable by
        // an identifier the principal may not see.
        let declared_scalars = &generation.bundle.manifest.declared_scalars;
        for partition in generation.bundle.partitions.values() {
            for slice_data in partition.slices.values() {
                // The permutation is the only entity→row bridge (I4, §5.1) — an O(1)
                // bounds-checked slot read, not a scan.
                let Some(row) = slice_data.permutation.row_of(entity) else {
                    continue;
                };
                // Phase 1 always has exactly one segment per (partition, slice) (R4 — the same
                // invariant `Engine::viewport`'s `MultiSegmentSlice` guard rests on); the
                // permutation addresses that single segment's row space.
                let Some(segment) = slice_data.segments.first() else {
                    continue;
                };
                return Ok(Some(ItemOut {
                    scalars: row_to_point(segment, row.raw(), declared_scalars).scalars,
                    external_id: self.external_id_of(entity)?, // N-3: propagate, never swallow
                }));
            }
        }
        // Visible in entity space but with no row anywhere: a buffered item awaiting flush. Same
        // `Ok(None)`, same 404 — it has no geometry to return.
        Ok(None)
    }
}

impl Engine {
    /// The masked viewport query. `slice` names a slice id; `zoom` is the tile depth (0–16);
    /// `bbox` is `[x0, y0, x1, y1]` in the bundle's declared extent; `k` caps points sampled per
    /// tile (clamped to `config.max_k` defensively); `pin` optionally re-pins geometry to a prior
    /// response's `(prefix, segments_version)`.
    pub fn viewport(
        &self,
        session: &Session,
        slice: &str,
        zoom: u8,
        bbox: [f64; 4],
        k: usize,
        pin: Option<PinId>,
    ) -> Result<ViewportOut> {
        // Load the generation pointer exactly once, at request start (see `GenerationHandle`'s
        // doc at its definition) — every subsequent read below (pin check, row-projection cache,
        // composition) comes from this one snapshot, so a concurrent overlay/bundle swap
        // mid-request can never mix state from two generations.
        let generation = self.generation.load_full();

        let effective_pin = match pin {
            Some(presented) => {
                // I11 / lifecycle §2.3: a pin is geometry identity only — `(prefix,
                // segments_version)` — never `overlay_version`. An overlay swap (any accepted
                // suppression/delete/predicate change) must not invalidate this pin; only a
                // bundle swap (new `prefix`/`segments_version`) does.
                if presented.prefix != generation.prefix
                    || presented.segments_version != generation.segments_version
                {
                    return Err(EngineError::PinExpired);
                }
                presented
            }
            None => PinId {
                prefix: generation.prefix.clone(),
                segments_version: generation.segments_version,
            },
        };

        let k = k.min(self.config.max_k);

        let slice_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.slices.get(slice))
            .ok_or_else(|| EngineError::UnknownSlice(slice.to_string()))?;

        // Fail closed on more than one segment (see `EngineError::MultiSegmentSlice`'s doc):
        // `tile_ranges` returns segment-local row indices, but `mask` is built from the slice's
        // single `Permutation`, which addresses exactly one segment's row space. Phase 1's build
        // never produces more than one, so this is not reachable today — but silently iterating
        // "just in case" would mis-count/mis-index the moment it became reachable, which is worse
        // than refusing outright.
        if slice_data.segments.len() > 1 {
            return Err(EngineError::MultiSegmentSlice(slice.to_string()));
        }
        let segment = slice_data.segments.first();

        let cache_key = (
            session.token_id,
            slice.to_string(),
            generation.segments_version,
        );
        let base: Arc<RowProjection> = {
            let mut cache = self.row_projection_cache.lock().unwrap();
            match cache.get(&cache_key) {
                Some(existing) => Arc::clone(existing),
                None => {
                    // Crosses entity space into row space over the *whole* fragment
                    // (`Permutation::project`'s cost note: seconds at 10⁹ rows) — paid once per
                    // (token, slice, segments_version) and cached here, never recomputed on a
                    // per-viewport path (shared-context constraint 8).
                    let projected = Arc::new(RowProjection::new(
                        &session.fragment,
                        &slice_data.permutation,
                    ));
                    cache.insert(cache_key, Arc::clone(&projected));
                    projected
                }
            }
        };

        let mask = compose(
            &session.fragment,
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            base,
            &slice_data.permutation,
        );

        let q = &generation.bundle.manifest.quantisation;
        let extent = Extent {
            x_min: q.x_min,
            x_max: q.x_max,
            y_min: q.y_min,
            y_max: q.y_max,
        };

        let tiles = tiles_for_bbox(bbox, zoom, &extent);
        let declared_scalars = &generation.bundle.manifest.declared_scalars;

        let mut tile_counts = Vec::new();
        let mut points = Vec::new();

        // A slice with zero segments (an empty build) has nothing visible in any tile; the loop
        // below simply never finds a non-empty range in that case.
        for tile in tiles {
            let Some(segment) = segment else { continue };

            let range = tile_ranges(segment, &tile);
            let visible = mask.count_range(range.clone());
            if visible == 0 {
                // Skip empty: no count row, no sampling work for a tile with nothing visible.
                continue;
            }

            tile_counts.push(TileCount {
                tile: tile.prefix,
                visible,
                // Phase 1 has no filters (Reference Sheet R5): matched == visible everywhere.
                matched: visible,
            });

            sample_tile(&mask, segment, range, declared_scalars, k, &mut points);
        }

        Ok(ViewportOut {
            pin: effective_pin,
            tiles: tile_counts,
            points,
        })
    }
}

/// **Placeholder sampler (I7) — deliberately wrong.** Takes the first `k` visible row IDs in
/// ascending row (Morton) order within `range`. This is *not* the priority-sample definition
/// (Reference Sheet R3: `priority(e) = splitmix64(e) >> 48`); it exists only so the walking
/// skeleton has an end-to-end query path to test against, and Phase 2's differential oracle is
/// *expected* to disagree with it. Do not let this drift into being mistaken for the real
/// sampling policy — replace it before Phase 2 ships.
fn sample_tile(
    mask: &EffectiveMask,
    segment: &SegmentData,
    range: Range<u32>,
    declared_scalars: &[DeclaredScalar],
    k: usize,
    out: &mut Vec<PointOut>,
) {
    let mut remaining = k;
    for row in mask.iter_range(range) {
        if remaining == 0 {
            break;
        }
        out.push(row_to_point(segment, row, declared_scalars));
        remaining -= 1;
    }
}

/// Gather one row's `entity_id`/`x`/`y`/declared scalars through `ColumnsRef` — zero-copy reads,
/// no per-row allocation beyond what a `Utf8` scalar's owned `String` requires.
fn row_to_point(segment: &SegmentData, row: u32, declared: &[DeclaredScalar]) -> PointOut {
    let idx = row as usize;
    let cols = &segment.columns;
    let tessera_id = TesseraId::new(cols.tessera_id()[idx]);
    let x = cols.x()[idx];
    let y = cols.y()[idx];

    let mut scalars = Vec::with_capacity(declared.len());
    for declared_scalar in declared {
        // A declared scalar absent from this segment's schema (shouldn't happen once the build
        // pipeline writes declared columns, but Phase 1's build never does yet) is skipped rather
        // than treated as an error — nothing here is authorisation-relevant.
        if let Some(value) = cols.scalar(&declared_scalar.name) {
            scalars.push(match value {
                ScalarSlice::U64(s) => ScalarOut::U64(s[idx]),
                ScalarSlice::F32(s) => ScalarOut::F32(s[idx]),
                ScalarSlice::Utf8(arr) => ScalarOut::Utf8(arr.value(idx).to_string()),
            });
        }
    }

    PointOut {
        tessera_id,
        x,
        y,
        scalars,
    }
}
