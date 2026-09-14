//! **The post-bundle artifact pass** — the build choosing each level's serving layout and writing
//! the derived structures for it, in the prefix it has just finished.
//!
//! # The defect this closes
//!
//! [Decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
//! says the layout is chosen **at the build** and re-evaluated at every fold. Until this existed
//! only the second half happened: `layers::publish` registers each level at its pin or at
//! artifact-major and stops, because it runs at pipeline stage 8 — before the tiler sort — and
//! *there is no row space yet*. Every observation the pick reads is a fact about where the data
//! landed in row order, and at stage 8 nothing has landed.
//!
//! What that cost is measured: `docs/evidence/memos/2026-08-22-artifact-scale-campaign.md`'s
//! finding 1 and §6. A freshly built bundle served its two enumerated layers artifact-major against
//! its own reported statistics, and the first fold flipped both — **2 430 → 221 ms** and
//! **1 464 → 139 ms**, eleven and ten and a half times, paid by every deployment between its build
//! and its first fold. Beside it, finding 2: because the build wrote none of the derived files, the
//! first request naming such a level built the row form *inside the response* and was truncated at
//! the 60-second whole-stream deadline.
//!
//! # Where it runs, and why there
//!
//! **After the segment write and before the manifests** (pipeline step 10.5). By then the
//! permutation and `row_entity` are on disk and fsynced, so a `RowSpace` over the published bundle
//! exists; the record batch and the sort's scratch have been dropped, so the pass runs past the
//! build's residency peak rather than on top of it. And the manifests have not been written, which
//! is what makes this **one** manifest write rather than two: the extents this produces and the
//! layouts it records go into the same `SegmentsManifest` the build was always going to write.
//!
//! # One implementation, not a second transcription
//!
//! Every byte written here comes from [`tessera_store::membership`] — the same functions the fold's
//! artifact pass calls, beside the formats they produce. This module supplies the walks (a level's
//! records against the row space) and the coordinates; it packs nothing and names no file itself.
//! A build and a fold therefore cannot file the same structure two ways, which is the failure a
//! second copy of the naming rule would have made available.
//!
//! # What a failure does
//!
//! **Reports and continues.** Every structure here is derived — a level without one composes it on
//! first use, which is what every request did before any of this existed — so a permutation that
//! will not reload, a directory that will not fsync or a column that will not compose costs a
//! recomputation and never a refusal. The pass reports what it produced, including nothing.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use croaring::Bitmap;
use tessera_authz::postings::{PostingRef, PostingsReader};
use tessera_lifecycle::membership::ArtifactStore;
use tessera_plugin::Plugin;
use tessera_store::derived::{resolve_segment, HeldShape, ShapeIndex};
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
// The derived structures' writer half lives beside the formats it writes; the alias is what keeps
// the call sites below reading as what they do rather than as which file they are in.
use tessera_store::derived;
use tessera_store::derived::{DerivedIndex, Filed, LevelShape, PostingSlice, SignatureIndex};
use tessera_store::permutation::ProjectScratch;
use tessera_store::RowSpace;
use tessera_types::layer::{MembershipSource, RegisteredLayer, ServingLayout};

use crate::layers::PublishedLayers;

/// One `(view, layer, level)`'s observed shape and the form it was recorded in — decision 0092's
/// (c), discharged at build time at last.
#[derive(Debug, Clone)]
pub struct LevelLayoutReport {
    pub view: String,
    pub layer: String,
    pub level: u32,
    /// The level's declared zoom range, which since 2026-08-28 decides whether a request that names
    /// no `levels` is answered at it. Reported so a gap or an inverted range is visible at the build
    /// rather than as a blank map at one zoom band.
    pub zoom: Option<(u32, u32)>,
    pub shape: LevelShape,
    /// How many artifacts the level's registry holds, which is **not** [`LevelShape::artifacts`]:
    /// the shape counts the ones whose stored membership the walk found rows in, and an attribute
    /// predicate's membership is not stored at all (see [`Self::observed`]).
    pub registered: u64,
    /// Whether [`Self::shape`] describes anything. False for an attribute predicate, whose members
    /// *are* the value column and are evaluated per request, so the walk this pass makes finds no
    /// rows and every figure in the shape comes out zero — for a level that holds its artifacts and
    /// serves them. Reporting those zeros reads as an empty layer and is how an hour was spent
    /// looking for a defect in a layer that was working (2026-08-28, the Overture rung).
    pub observed: bool,
    pub pinned: bool,
    pub chosen: ServingLayout,
}

impl LevelLayoutReport {
    /// The artifact count to *report* — the observed one where there is one, and the registry's
    /// otherwise. Never the input to a layout choice, which stays [`LevelShape::artifacts`].
    fn artifacts(&self) -> u64 {
        if self.observed {
            self.shape.artifacts
        } else {
            self.registered
        }
    }
}

/// What the pass produced, for the manifest and for the report.
#[derive(Default)]
pub struct ArtifactPass {
    pub tile_index_extents: Vec<tessera_store::manifest::TileIndexExtent>,
    pub row_column_extents: Vec<tessera_store::manifest::RowColumnExtent>,
    /// The persisted row form of every spatial level that got no column — see
    /// `ShapeRowsExtent`.
    pub shape_rows_extents: Vec<tessera_store::manifest::ShapeRowsExtent>,
    /// Every spatial level's decompositions, so the engine's open assembles rather than descends.
    pub shape_held_extents: Vec<tessera_store::manifest::ShapeHeldExtent>,
    /// Every file this pass wrote, for `MANIFEST.files` — an undigested file is one a torn write
    /// cannot be attributed to.
    pub paths: Vec<std::path::PathBuf>,
    pub levels: Vec<LevelLayoutReport>,
    /// Per spatial level, what resolving the build's segment against its shapes cost
    /// (`polygon-membership.md` §9's per-flush row, measured here over the one segment a build
    /// writes — decision 0091: the build does what the flush does).
    pub resolutions: Vec<(String, u32, crate::shapes::ResolutionReport)>,
    /// The pass's own wall time. Reported because it is new work at the end of every build and an
    /// operator should not have to infer it from the total.
    pub elapsed_ms: u64,
}

/// Choose every level's layout, write the derived structures for it, and record both.
///
/// `published` is edited in place: the chosen layouts replace the registered records the build
/// wrote at stage 8, so the manifest assembled after this carries them without a second write.
#[allow(clippy::too_many_arguments)]
pub fn run(
    published: &mut PublishedLayers,
    store: &ArtifactStore,
    prefix_dir: &Path,
    partition: &str,
    view: &str,
    row_count: u32,
    scratch_dir: &Path,
    index: &mut DerivedIndex,
) -> ArtifactPass {
    let started = Instant::now();
    let mut pass = ArtifactPass::default();
    // **One set of projection buffers for the whole pass.** Every walk below projects a membership
    // per artifact, and `Permutation::project` allocates and zeroes a 512 KB stamp on each call —
    // it is written for one call per session and says so. Shared through a `RefCell` because the
    // walks are `Fn` closures.
    let scratch = std::cell::RefCell::new(ProjectScratch::default());

    // **Only the layers drawn on this view.** A layer appears in the views it declares and no
    // others (`views.md` §3.5), so a pass over a view a layer does not name would write that
    // layer an extent in a row space it is not drawn in — which the serving path would then
    // answer from.
    let drawn: std::collections::BTreeSet<&str> = published
        .layers
        .iter()
        .filter(|layer| {
            layer
                .declaration
                .views
                .iter()
                .any(|declared| declared == view)
        })
        .map(|layer| layer.declaration.name.as_str())
        .collect();
    // **An artifact of a scoped layer is drawn in its own view and in no other** (`views.md`
    // §3.5): its membership is entity space and every view holds some of those entities, so a
    // Q1 cluster projected into Q2's row space would be a real, wrong artifact there. The view's
    // own key is what an artifact names, so a group's several layouts over one key set — `quarter`
    // and `quarter_alt` — draw the same artifact in each.
    let view_key = tessera_store::view_path_components(view)
        .last()
        .copied()
        .unwrap_or(view)
        .to_string();
    let elsewhere = |layer: &str, level: u32, ordinal: u32| -> bool {
        published
            .artifact_views
            .get(layer)
            .and_then(|artifacts| artifacts.get(&(level, ordinal)))
            .is_some_and(|owner| owner != &view_key)
    };

    let levels: Vec<(String, u32)> = store
        .levels_and_extents()
        .filter(|(layer, _, _)| drawn.contains(layer))
        .map(|(layer, level, _)| (layer.to_string(), level))
        .collect();
    if levels.is_empty() {
        return pass;
    }

    // **The row space of the bundle this build just wrote**, opened from the file rather than kept
    // from the sort: the permutation is fsynced by now, and reading it back is what makes this pass
    // a function of the published prefix rather than of a structure that only existed in memory.
    let permutation_path =
        tessera_store::view_path(&prefix_dir.join("partitions").join(partition), view)
            .join("permutation.bin");
    let space = match tessera_store::Permutation::load(&permutation_path) {
        Ok(permutation) => RowSpace::new(std::sync::Arc::new(permutation), row_count),
        Err(error) => {
            eprintln!(
                "artifact pass: the permutation this build just wrote would not reload ({error}); \
                 every level is recorded artifact-major and its structures are derived on first \
                 request"
            );
            pass.elapsed_ms = started.elapsed().as_millis() as u64;
            return pass;
        }
    };

    // ---- the pick, per level ----------------------------------------------------------------
    let by_layer: BTreeMap<String, RegisteredLayer> = published
        .layers
        .iter()
        .map(|layer| (layer.declaration.name.clone(), layer.clone()))
        .collect();

    // ---- the shape layers' memberships, resolved over the segment this build wrote -------------
    //
    // **The build does what the flush does** (decision 0091; `polygon-membership.md` §6.3): every
    // row of the one segment is resolved against each spatial level's shapes — interior tiles as
    // whole ranges, boundary-cell rows one by one — and the per-row source that produces is what
    // the pick observes and the column is composed from below, exactly as an enumerated level's
    // member table is. The serving engine resolves the same segment again at open from the same
    // shapes (`tessera_engine::shapes`), so nothing here is persisted but the layout and the
    // column; what this pass buys is the pick and the report.
    let segment = load_build_segment(prefix_dir, partition, view, row_count);
    let mut resolved: BTreeMap<(String, u32), Vec<Option<Bitmap>>> = BTreeMap::new();
    let mut decomposed: BTreeMap<(String, u32), Vec<Option<HeldShape>>> = BTreeMap::new();
    for (layer, level) in &levels {
        let Some(registered) = by_layer.get(layer) else {
            continue;
        };
        let spatial = registered.declaration.membership == MembershipSource::Spatial
            && registered.declaration.shape.is_some();
        if !spatial {
            continue;
        }
        let Some(segment) = &segment else {
            continue;
        };
        let started = Instant::now();
        let ordinals = store
            .level(layer, *level)
            .map(|(o, _)| o as usize + 1)
            .max()
            .unwrap_or(0);
        let shapes: Vec<Option<HeldShape>> = (0..ordinals as u32)
            .map(|ordinal| {
                store
                    .shape_of(layer, *level, ordinal)
                    .and_then(|shapes| shapes.for_view(view))
                    .and_then(|bytes| HeldShape::from_bytes(bytes).ok())
            })
            .collect();
        let index = ShapeIndex::build(&shapes);
        let out = resolve_segment(segment, &shapes, &index);
        pass.resolutions.push((
            layer.clone(),
            *level,
            crate::shapes::ResolutionReport {
                rows: u64::from(segment.row_count),
                rows_tested: out.rows_tested,
                rows_interior: out.rows_interior,
                artifacts_empty: out.artifacts_empty,
                empty_keys: out
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, rows)| rows.as_ref().is_some_and(Bitmap::is_empty))
                    .filter_map(|(ordinal, _)| {
                        store
                            .level(layer, *level)
                            .find(|(o, _)| *o as usize == ordinal)
                            .and_then(|(_, record)| record.key.clone())
                    })
                    .take(EMPTY_KEYS_REPORTED)
                    .collect(),
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
        ));
        resolved.insert((layer.clone(), *level), out.rows);
        decomposed.insert((layer.clone(), *level), shapes);
    }
    let walk_resolved = |rows: &[Option<Bitmap>], visit: &mut dyn FnMut(u32, &Bitmap)| {
        for (ordinal, rows) in rows.iter().enumerate() {
            if let Some(rows) = rows {
                visit(ordinal as u32, rows);
            }
        }
    };

    let mut chosen: Vec<(String, u32, ServingLayout)> = Vec::with_capacity(levels.len());
    for (layer, level) in &levels {
        let Some(registered) = by_layer.get(layer) else {
            continue;
        };
        // One membership at a time, exactly as the fold observes it: the figure is the same either
        // way and what differs is what is held while it runs.
        let shape = derived::observe_shape(space.base_rows(), &|visit| {
            if let Some(rows) = resolved.get(&(layer.clone(), *level)) {
                walk_resolved(rows, visit);
                return;
            }
            for (ordinal, record) in store.level(layer, *level) {
                if elsewhere(layer, *level, ordinal) {
                    continue;
                }
                visit(
                    ordinal,
                    &space.project_base_with(&record.members, &mut scratch.borrow_mut()),
                );
            }
        });
        let layout = derived::choose(&registered.declaration, shape);
        pass.levels.push(LevelLayoutReport {
            view: view.to_string(),
            layer: layer.clone(),
            level: *level,
            zoom: registered
                .declaration
                .levels
                .iter()
                .find(|l| l.level == *level)
                .and_then(|l| l.zoom),
            shape,
            registered: store.level(layer, *level).count() as u64,
            observed: !matches!(
                registered.declaration.membership,
                MembershipSource::Attribute(_)
            ),
            pinned: registered.declaration.layout.is_some(),
            chosen: layout,
        });
        chosen.push((layer.clone(), *level, layout));
    }

    // **Recorded before a byte is written**, which is the fold's own ordering and for the fold's
    // own reason: the files and the record are one publication, and a record taken afterwards could
    // describe a level the prefix does not carry.
    for registered in &mut published.layers {
        for (layer, level, layout) in &chosen {
            if &registered.declaration.name != layer {
                continue;
            }
            let idx = *level as usize;
            if registered.layouts.len() <= idx {
                registered.layouts.resize(idx + 1, ServingLayout::default());
            }
            registered.layouts[idx] = *layout;
        }
    }

    // ---- the tile indexes: every level that is not row-major -----------------------------------
    //
    // **A row-major level has nothing to index** (selection memo §1): its candidacy is a scan of
    // `viewport ∩ M_auth`, which the viewport already bounds.
    let mut tile_indexes: Vec<Filed> = Vec::new();
    for (layer, level, layout) in &chosen {
        if layout.is_row_major() {
            continue;
        }
        let ordinals = store.level(layer, *level).count() as u32;
        tile_indexes.push(Filed {
            view: view.to_string(),
            // **A build coins each key once, so every structure it writes is the declared
            // incarnation** (decision 0115). There is no drop at a build to leave a predecessor.
            incarnation: tessera_store::manifest::DECLARED_INCARNATION,
            layer: layer.clone(),
            level: *level,
            level_version: store.level_version(layer, *level),
            layout: *layout,
            bytes: derived::FiledBytes::InHand(derived::project_tile_index(
                ordinals,
                space.base_rows(),
                &|visit| {
                if let Some(rows) = resolved.get(&(layer.clone(), *level)) {
                    walk_resolved(rows, visit);
                    return;
                }
                for (ordinal, record) in store.level(layer, *level) {
                    if elsewhere(layer, *level, ordinal) {
                        continue;
                    }
                    visit(
                        ordinal,
                        &space.project_base_with(&record.members, &mut scratch.borrow_mut()),
                    );
                }
                },
            )),
        });
    }
    pass.tile_index_extents =
        derived::file_tile_indexes(prefix_dir, partition, MANIFEST_N, index, tile_indexes);

    // ---- the row-major columns: every level that is ---------------------------------------------
    //
    // ⊘ **An attribute level's column is not this pass's to write**, for the reason the fold gives:
    // its labels come from the value column the predicate names, and this composes from stored
    // memberships — which such a level has none of. Composing anyway would write a file of nothing
    // but holes and leave a reader adopting a column no request will claim. A spatial level's
    // column is composed from the rows resolved above.
    let mut columns: Vec<Filed> = Vec::new();
    for (layer, level, layout) in &chosen {
        if !layout.is_row_major() {
            continue;
        }
        let composable = by_layer.get(layer).is_some_and(|registered| {
            matches!(
                registered.declaration.membership,
                MembershipSource::Enumerated | MembershipSource::Spatial
            )
        });
        if !composable {
            continue;
        }
        let ordinals = store.level(layer, *level).count() as u32;
        let staged = derived::project_row_column(
            ordinals,
            space.base_rows(),
            *layout,
            scratch_dir,
            &|visit| {
                if let Some(rows) = resolved.get(&(layer.clone(), *level)) {
                    walk_resolved(rows, visit);
                    return;
                }
                for (ordinal, record) in store.level(layer, *level) {
                    if elsewhere(layer, *level, ordinal) {
                        continue;
                    }
                    visit(
                        ordinal,
                        &space.project_base_with(&record.members, &mut scratch.borrow_mut()),
                    );
                }
            },
        );
        match staged {
            Ok(Some(path)) => columns.push(Filed {
                view: view.to_string(),
                incarnation: tessera_store::manifest::DECLARED_INCARNATION,
                layer: layer.clone(),
                level: *level,
                level_version: store.level_version(layer, *level),
                layout: *layout,
                bytes: derived::FiledBytes::Staged(path),
            }),
            // The build-time half of the refusal the declaration could not make: whether an
            // attribute is single-valued is a property of the data, and so is whether a level's
            // entries fit the `u32` the list form's offsets are. The level is served
            // artifact-major, every answer unchanged.
            Ok(None) => eprintln!(
                "artifact pass: {layer} level {level} was recorded {} and {}, so it is served \
                 artifact-major. Every answer is unchanged; the layout is not",
                layout.pin_word(),
                match layout {
                    ServingLayout::RowMajorList =>
                        "its member entries outrun the u32 its offsets are",
                    _ => "its memberships do not partition",
                }
            ),
            // **A derived structure, so a failure is a dropped file and not a refused build.** The
            // level composes its column on first request, which is what every request did before
            // the file existed.
            Err(error) => eprintln!(
                "artifact pass: {layer} level {level}'s row-major column would not be composed \
                 ({error}); that level derives it on first use"
            ),
        }
    }
    pass.row_column_extents =
        derived::file_row_columns(prefix_dir, partition, MANIFEST_N, index, columns);

    // ---- the shape row forms: every spatial level whose persisted form is not a column ---------
    //
    // A level served row-major has its column above and the open inverts that; one served
    // artifact-major — and one recorded row-major whose column would not compose, which is served
    // artifact-major — has no other durable form of what was just resolved, so the row form is
    // written for it, keyed by the build's segment and the level's version.
    let mut shape_rows: Vec<derived::FiledShapeRows> = Vec::new();
    if let Some(segment) = &segment {
        for ((layer, level), rows) in &resolved {
            let has_column = pass
                .row_column_extents
                .iter()
                .any(|e| &e.layer == layer && e.level == *level && e.view == view);
            if has_column {
                continue;
            }
            let level_version = store.level_version(layer, *level);
            shape_rows.push(derived::FiledShapeRows {
                view: view.to_string(),
                incarnation: tessera_store::manifest::DECLARED_INCARNATION,
                layer: layer.clone(),
                level: *level,
                level_version,
                seg_id: segment.seg_id.clone(),
                row_count: segment.row_count,
                bytes: derived::shape_rows_bytes(
                    level_version,
                    &segment.seg_id,
                    segment.row_count,
                    rows,
                ),
            });
        }
    }
    pass.shape_rows_extents =
        derived::file_shape_rows(prefix_dir, partition, MANIFEST_N, index, shape_rows);

    // ---- the decompositions, per spatial level ------------------------------------------------
    let held: Vec<Filed> = decomposed
        .iter()
        .map(|((layer, level), shapes)| {
            let level_version = store.level_version(layer, *level);
            let entries: Vec<(Option<&[u8]>, Option<&HeldShape>)> = (0..shapes.len() as u32)
                .map(|ordinal| {
                    (
                        store
                            .shape_of(layer, *level, ordinal)
                            .and_then(|shapes| shapes.for_view(view)),
                        shapes[ordinal as usize].as_ref(),
                    )
                })
                .collect();
            Filed {
                view: view.to_string(),
                incarnation: tessera_store::manifest::DECLARED_INCARNATION,
                layer: layer.clone(),
                level: *level,
                level_version,
                layout: ServingLayout::ArtifactMajor,
                bytes: derived::FiledBytes::InHand(derived::shape_held_bytes(level_version, &entries)),
            }
        })
        .collect();
    pass.shape_held_extents =
        derived::file_shape_held(prefix_dir, partition, MANIFEST_N, index, held);

    for entry in &pass.tile_index_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    for entry in &pass.row_column_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    for entry in &pass.shape_rows_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    for entry in &pass.shape_held_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    pass.elapsed_ms = started.elapsed().as_millis() as u64;
    pass
}

/// The one segment a build writes, reopened from the prefix so the pass resolves the same bytes
/// the serving engine will. `None` — said so — where it will not reopen; the shape layers then
/// resolve at the engine's open and are recorded in their pinned or default layout.
fn load_build_segment(
    prefix_dir: &Path,
    partition: &str,
    view: &str,
    row_count: u32,
) -> Option<SegmentData> {
    let dir = tessera_store::view_path(&prefix_dir.join("partitions").join(partition), view)
        .join("segments")
        .join(crate::BUILD_SEG_ID);
    let morton = MortonSlice::load(&dir.join("morton.u32"));
    let cuts = tessera_store::read::CutIndex::load(
        &dir.join(tessera_store::read::CutIndex::FILE),
        row_count,
    );
    let columns = ColumnsRef::load(&dir.join("columns.arrow"));
    match (morton, cuts, columns) {
        (Ok(morton), Ok(cuts), Ok(columns)) => Some(SegmentData {
            seg_id: crate::BUILD_SEG_ID.to_string(),
            row_count,
            morton,
            cuts,
            columns,
        }),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
            eprintln!(
                "artifact pass: the segment this build just wrote would not reopen ({error}); \
                 every shape layer is recorded in its pinned or default layout and resolved at \
                 the engine's open"
            );
            None
        }
    }
}

/// How many of a level's row-less artifacts the report names. Enough to look into the count
/// without printing a level's worth of keys.
const EMPTY_KEYS_REPORTED: usize = 10;

/// The publication number every file this pass writes is named after.
///
/// **Zero, because a build is publication zero.** The fold names its files after the manifest
/// generation it is about to publish; a build writes `SEGMENTS-0.json` and has exactly one.
const MANIFEST_N: u64 = 0;

/// Compose and file this prefix's containment partitions, one per `(layer, level)`.
///
/// **Once for the prefix.** A partition is a function of the level's records and the prefix's
/// postings, so it is the same bytes whichever view is being written and its manifest entry
/// carries no view. Composing it inside [`run`], which runs per view, would write every level one
/// identical file and one entry per view. The fold composes for every level in one call
/// (`Executor::write_containment_partitions`) and this does the same, which is why the level list
/// here is the store's rather than one view's drawn layers.
///
/// **The gate first**: under any plugin but the builtin the partition is not sound at all, so
/// nothing is composed and nothing is written — the same gate the fold applies, taken from the same
/// manifest field.
pub fn containment(
    store: &ArtifactStore,
    prefix_dir: &Path,
    partition: &str,
    data_plugin_hash: &str,
    index: &mut DerivedIndex,
) -> Vec<tessera_store::manifest::ContainmentExtent> {
    if data_plugin_hash != tessera_plugin::Passthrough::new().data_plugin_hash() {
        return Vec::new();
    }
    let postings_path = prefix_dir
        .join("partitions")
        .join(partition)
        .join("terms")
        .join("postings.arrow");
    let postings = match PostingsReader::open(&postings_path, true) {
        Ok(postings) => postings,
        Err(error) => {
            eprintln!(
                "artifact pass: the postings this build just wrote would not open ({error}); every \
                 level composes its containment partition on first use"
            );
            return Vec::new();
        }
    };

    let levels: Vec<(String, u32)> = store
        .levels_and_extents()
        .map(|(layer, level, _)| (layer.to_string(), level))
        .collect();
    let mut composed: Vec<Filed> = Vec::new();
    for (layer, level) in &levels {
        let contents = |visit: &mut dyn FnMut(u32, &[&Bitmap])| {
            for (ordinal, record) in store.level(layer, *level) {
                let generating: Vec<&Bitmap> =
                    record.contents.iter().map(|c| &c.generated_from).collect();
                visit(ordinal, &generating);
            }
        };
        // **Every level, including one whose artifacts carry no content at all.** The fold composes
        // for every level and the reader adopts per level, so a level skipped here is one the first
        // request composes — an empty table, cheaply, but composed on the request path all the
        // same. `SignatureIndex::build` returns immediately on an empty set, so the cost of the
        // symmetry is a file header.
        let wanted = derived::generating_entities(&contents);
        // The one adapter between the postings format and the composer — `tessera-store` may not
        // depend on `tessera-authz`, so the shape is handed across and the walk is written once.
        // The engine holds the identical six lines (`containment::signature_index`).
        let signatures = SignatureIndex::build(&wanted, postings.term_count(), &|term, visit| {
            if let Some(posting) = postings.posting_at(term)? {
                match posting {
                    PostingRef::Array(bytes) => visit(PostingSlice::Array(bytes)),
                    PostingRef::Roaring(view) => visit(PostingSlice::Roaring(&view)),
                }
            }
            Ok(())
        });
        let signatures = match signatures {
            Ok(signatures) => signatures,
            Err(error) => {
                eprintln!(
                    "artifact pass: {layer} level {level}'s containment signatures would not be \
                     read ({error}); that level composes its partition on first use"
                );
                continue;
            }
        };
        composed.push(Filed {
            view: String::new(),
            incarnation: tessera_store::manifest::DECLARED_INCARNATION,
            layer: layer.clone(),
            level: *level,
            level_version: store.level_version(layer, *level),
            layout: ServingLayout::ArtifactMajor,
            bytes: derived::FiledBytes::InHand(derived::compose_containment(&contents, &signatures)),
        });
    }
    derived::file_containment(prefix_dir, partition, MANIFEST_N, index, composed)
}

/// The pass's own report, printed where `report_attribute_coverage` prints — so both entry points
/// into the build show it and neither has to reconstruct it.
///
/// **Both figures, and only one of them decides.** The `everywhere` fraction is the trigger
/// (`tessera_store::derived::ROW_MAJOR_EVERYWHERE_FRACTION`); blocks per artifact is decision
/// 0092's (c) and is reported beside it, because it is what says how much *work* a membership is
/// even now that it no longer says how far that work is spread.
pub fn report(pass: &ArtifactPass) {
    if pass.levels.is_empty() {
        return;
    }
    eprintln!(
        "artifact layouts, chosen from the bundle's own row space ({} ms):",
        pass.elapsed_ms
    );
    for level in &pass.levels {
        // **A level whose membership is not stored has no shape to print**, and printing the zeros
        // the walk returned would say it holds nothing. Its artifacts are its column's values and
        // its form follows from the membership, so the count and the form are the whole of what
        // there is to say.
        if !level.observed {
            eprintln!(
                "  {} level {} [{}]: {} artifact(s) from its column — served {}; no spread to \
                 observe, the membership being the column rather than a stored bitmap",
                level.layer,
                level.level,
                level.view,
                level.registered,
                level.chosen.pin_word(),
            );
            continue;
        }
        eprintln!(
            "  {} level {} [{}]: {} artifact(s) with rows, {:.3} everywhere, {:.1} \
             blocks/artifact, {} — served {}{}",
            level.layer,
            level.level,
            level.view,
            level.shape.artifacts,
            level.shape.everywhere_fraction,
            level.shape.blocks_per_artifact,
            if level.shape.partitions {
                "disjoint"
            } else {
                "overlapping"
            },
            level.chosen.pin_word(),
            if level.pinned { " (pinned)" } else { "" },
        );
    }
    for (layer, level, r) in &pass.resolutions {
        eprintln!(
            "  {layer} level {level}: resolved the build's segment against its shapes — {} row(s), \
             {} admitted from interior tiles, {} tested one by one in boundary cells; {} \
             artifact(s) hold a shape and no row of it; {} ms",
            r.rows, r.rows_interior, r.rows_tested, r.artifacts_empty, r.elapsed_ms
        );
        if !r.empty_keys.is_empty() {
            eprintln!(
                "    with a shape and no row{}: {}",
                if r.artifacts_empty as usize > r.empty_keys.len() {
                    format!(" (first {} of {})", r.empty_keys.len(), r.artifacts_empty)
                } else {
                    String::new()
                },
                r.empty_keys.join(", ")
            );
        }
    }
    eprintln!(
        "  wrote {} tile index(es), {} row-major column(s), {} shape row form(s), {} \
         decomposition file(s)",
        pass.tile_index_extents.len(),
        pass.row_column_extents.len(),
        pass.shape_rows_extents.len(),
        pass.shape_held_extents.len(),
    );

    // **What a whole-layer response costs, reported and never refused**
    // ([decision 0103](../../../docs/decisions/0103-a-request-naming-no-levels-is-answered-at-the-declared-ones.md),
    // owner ruling 2026-08-28). The lines above give each level its own count; this is the sum per
    // layer, and the sum is what predicts response volume, a response carrying one row per served
    // artifact.
    //
    // **This is the whole of what replaces an artifact ceiling.** A large response is slow, not
    // wrong: it discloses nothing the mask did not already allow and a rerun costs nothing, so it
    // is the operator's call. What bounds it is the request's `levels`, whose absent case follows
    // the zoom ranges printed here — so an operator who does not like a number has a declaration to
    // change, and this is where they see it.
    //
    // **No byte estimate.** Bytes per artifact follow what the layer declares — a count-only level
    // is tens of bytes and one declaring a hull is unbounded, the rings being a function of the
    // membership — so a constant would be a guess wearing a measurement's clothes.
    let mut by_layer: std::collections::BTreeMap<&str, Vec<&LevelLayoutReport>> =
        std::collections::BTreeMap::new();
    for level in &pass.levels {
        by_layer
            .entry(level.layer.as_str())
            .or_default()
            .push(level);
    }
    for (layer, mut levels) in by_layer {
        // **Only where there is more than one level**, because for a single-level layer the sum is
        // the line already printed above and repeating it is noise — and noise here costs the same
        // as anywhere else: a report that says something about every layer stops being read.
        if levels.len() < 2 {
            continue;
        }
        levels.sort_by_key(|l| l.level);
        let total: u64 = levels.iter().map(|l| l.artifacts()).sum();
        let ranges = levels.iter().any(|l| l.zoom.is_some());
        if ranges {
            eprintln!(
                "  {layer}: {total} artifact(s) across {} levels — what a response naming it \
                 carries at `levels: \"all\"`. Omitting `levels` serves the levels whose declared \
                 zoom range covers the request's depth:",
                levels.len()
            );
        } else {
            // No range on any level, so there is no map for the *absent* case to follow and it
            // serves all of them — the total is what an ordinary request pays. An explicit
            // `levels` still selects here, the layer having levels to select; what it has no
            // default for is the omitted case.
            eprintln!(
                "  {layer}: {total} artifact(s) across {} levels, none declaring a zoom range — so \
                 a response omitting `levels` carries all of them. Declare a range per level to \
                 bound it:",
                levels.len()
            );
        }
        for l in levels {
            match l.zoom {
                Some((lo, hi)) => eprintln!(
                    "    level {} [{}]: {} artifact(s), zoom {lo}–{hi}",
                    l.level,
                    l.view,
                    l.artifacts()
                ),
                // A level with no range of its own beside levels that have one is served at every
                // depth: it has no scale to be outside of.
                None => eprintln!(
                    "    level {} [{}]: {} artifact(s), no zoom range — served at every depth",
                    l.level,
                    l.view,
                    l.artifacts()
                ),
            }
        }
    }
}
