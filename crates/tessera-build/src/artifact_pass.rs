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
// The derived structures' writer half lives beside the formats it writes; the alias is what keeps
// the call sites below reading as what they do rather than as which file they are in.
use tessera_store::membership as derived;
use tessera_store::membership::{Filed, LevelShape, PostingSlice, SignatureIndex};
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
    pub shape: LevelShape,
    pub pinned: bool,
    pub chosen: ServingLayout,
}

/// What the pass produced, for the manifest and for the report.
#[derive(Default)]
pub struct ArtifactPass {
    pub tile_index_extents: Vec<tessera_store::manifest::TileIndexExtent>,
    pub row_column_extents: Vec<tessera_store::manifest::RowColumnExtent>,
    pub containment_extents: Vec<tessera_store::manifest::ContainmentExtent>,
    /// Every file this pass wrote, for `MANIFEST.files` — an undigested file is one a torn write
    /// cannot be attributed to.
    pub paths: Vec<std::path::PathBuf>,
    pub levels: Vec<LevelLayoutReport>,
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
    data_plugin_hash: &str,
) -> ArtifactPass {
    let started = Instant::now();
    let mut pass = ArtifactPass::default();

    let levels: Vec<(String, u32)> = store
        .levels_and_extents()
        .map(|(layer, level, _)| (layer.to_string(), level))
        .collect();
    if levels.is_empty() {
        return pass;
    }

    // **The row space of the bundle this build just wrote**, opened from the file rather than kept
    // from the sort: the permutation is fsynced by now, and reading it back is what makes this pass
    // a function of the published prefix rather than of a structure that only existed in memory.
    let permutation_path = prefix_dir
        .join("partitions")
        .join(partition)
        .join("views")
        .join(view)
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
    let mut chosen: Vec<(String, u32, ServingLayout)> = Vec::with_capacity(levels.len());
    for (layer, level) in &levels {
        let Some(registered) = by_layer.get(layer) else {
            continue;
        };
        // One membership at a time, exactly as the fold observes it: the figure is the same either
        // way and what differs is what is held while it runs.
        let shape = derived::observe_shape(space.base_rows(), &|visit| {
            for (ordinal, record) in store.level(layer, *level) {
                visit(ordinal, &space.project_base(&record.members));
            }
        });
        let layout = derived::choose(&registered.declaration, shape);
        pass.levels.push(LevelLayoutReport {
            view: view.to_string(),
            layer: layer.clone(),
            level: *level,
            shape,
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
            layer: layer.clone(),
            level: *level,
            level_version: store.level_version(layer, *level),
            layout: *layout,
            bytes: derived::project_tile_index(ordinals, space.base_rows(), &|visit| {
                for (ordinal, record) in store.level(layer, *level) {
                    visit(ordinal, &space.project_base(&record.members));
                }
            }),
        });
    }
    pass.tile_index_extents =
        derived::file_tile_indexes(prefix_dir, partition, MANIFEST_N, tile_indexes);

    // ---- the row-major columns: every level that is ---------------------------------------------
    //
    // ⊘ **A predicate level's column is not this pass's to write**, for the reason the fold gives:
    // an attribute layer's labels come from the value column the predicate names, and this composes
    // from stored memberships — which such a level has none of. Composing anyway would write a file
    // of nothing but holes and leave a reader adopting a column no request will claim.
    let mut columns: Vec<Filed> = Vec::new();
    for (layer, level, layout) in &chosen {
        if !layout.is_row_major() {
            continue;
        }
        let enumerated = by_layer.get(layer).is_some_and(|registered| {
            matches!(
                registered.declaration.membership,
                MembershipSource::Enumerated
            )
        });
        if !enumerated {
            continue;
        }
        let ordinals = store.level(layer, *level).count() as u32;
        let bytes = derived::project_row_column(ordinals, space.base_rows(), *layout, &|visit| {
            for (ordinal, record) in store.level(layer, *level) {
                visit(ordinal, &space.project_base(&record.members));
            }
        });
        match bytes {
            Some(bytes) => columns.push(Filed {
                view: view.to_string(),
                layer: layer.clone(),
                level: *level,
                level_version: store.level_version(layer, *level),
                layout: *layout,
                bytes,
            }),
            // The build-time half of the refusal the declaration could not make: whether an
            // attribute is single-valued is a property of the data. The level is served
            // artifact-major, every answer unchanged.
            None => eprintln!(
                "artifact pass: {layer} level {level} was recorded {} and its memberships do not \
                 partition, so it is served artifact-major. Every answer is unchanged; the layout \
                 is not",
                layout.pin_word()
            ),
        }
    }
    pass.row_column_extents = derived::file_row_columns(prefix_dir, partition, MANIFEST_N, columns);

    // ---- the containment partitions -------------------------------------------------------------
    pass.containment_extents = containment(store, &levels, prefix_dir, partition, data_plugin_hash);

    for entry in &pass.tile_index_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    for entry in &pass.row_column_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    for entry in &pass.containment_extents {
        pass.paths.push(prefix_dir.join(&entry.path));
    }
    pass.elapsed_ms = started.elapsed().as_millis() as u64;
    pass
}

/// The publication number every file this pass writes is named after.
///
/// **Zero, because a build is publication zero.** The fold names its files after the manifest
/// generation it is about to publish; a build writes `SEGMENTS-0.json` and has exactly one.
const MANIFEST_N: u64 = 0;

/// Compose and file this prefix's containment partitions, one per `(layer, level)`.
///
/// **The gate first**: under any plugin but the builtin the partition is not sound at all, so
/// nothing is composed and nothing is written — the same gate the fold applies, taken from the same
/// manifest field.
fn containment(
    store: &ArtifactStore,
    levels: &[(String, u32)],
    prefix_dir: &Path,
    partition: &str,
    data_plugin_hash: &str,
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

    let mut composed: Vec<Filed> = Vec::new();
    for (layer, level) in levels {
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
            layer: layer.clone(),
            level: *level,
            level_version: store.level_version(layer, *level),
            layout: ServingLayout::ArtifactMajor,
            bytes: derived::compose_containment(&contents, &signatures),
        });
    }
    derived::file_containment(prefix_dir, partition, MANIFEST_N, composed)
}

/// The pass's own report, printed where `report_attribute_coverage` prints — so both entry points
/// into the build show it and neither has to reconstruct it.
///
/// **Both figures, and only one of them decides.** The `everywhere` fraction is the trigger
/// (`tessera_store::membership::ROW_MAJOR_EVERYWHERE_FRACTION`); blocks per artifact is decision
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
        eprintln!(
            "  {} level {} [{}]: {} artifact(s), {:.3} everywhere, {:.1} blocks/artifact, \
             {} — served {}{}",
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
    eprintln!(
        "  wrote {} tile index(es), {} row-major column(s), {} containment partition(s)",
        pass.tile_index_extents.len(),
        pass.row_column_extents.len(),
        pass.containment_extents.len(),
    );
}
