//! Opening the measurement corpus: a built bundle's membership pack and the row locator that
//! resolves a member row to a position.
//!
//! Shared by the two ignored measurement binaries (`hull_geometry.rs`, `hull_triangulation.rs`)
//! because both answer questions about the same 197 memberships and a second copy of the loader is
//! a second place for the view name or the pack path to drift.

#![allow(dead_code)]

use croaring::Bitmap;
use std::path::{Path, PathBuf};
use tessera_engine::derived::RowLocator;
use tessera_store::read::{open_bundle, Bundle, SegmentData};

/// The locator over a leaked bundle, and one visible-row bitmap per artifact, in pack order.
pub struct Corpus {
    pub locator: RowLocator<'static>,
    pub memberships: Vec<(u32, Bitmap)>,
}

/// Opens the bundle named by `TESSERA_HULL_BUNDLE`, with the pack, partition and view names the
/// other environment variables override.
///
/// **The bundle is leaked on purpose and never reclaimed.** The locator borrows segments out of it
/// for the whole run, and a measurement binary that opens one bundle and exits has nothing to gain
/// from threading that lifetime through every timing loop.
pub fn open() -> Corpus {
    let Ok(root) = std::env::var("TESSERA_HULL_BUNDLE") else {
        panic!("set TESSERA_HULL_BUNDLE to a bundle root (the directory holding CURRENT)");
    };
    let root = PathBuf::from(root);
    let layer_pack = std::env::var("TESSERA_HULL_PACK")
        .unwrap_or_else(|_| "partitions/default/members/members-000000-000.tsmb".into());
    let partition = std::env::var("TESSERA_HULL_PARTITION").unwrap_or_else(|_| "default".into());
    let view_name = std::env::var("TESSERA_HULL_VIEW").unwrap_or_else(|_| "s0".into());

    let bundle = Box::new(open_bundle(&root).expect("bundle opens"));
    let bundle: &'static Bundle = Box::leak(bundle);
    // The pack path in the side-manifest is relative to the published prefix, which `CURRENT` names.
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT")).unwrap();
    let prefix = root.join(current["prefix"].as_str().expect("prefix"));
    let part = &bundle.partitions[&partition];
    let view = &part.views[&view_name];

    let row_bases: std::collections::HashMap<&str, u32> = view
        .row_space
        .extents()
        .iter()
        .map(|e| (e.seg_id.as_str(), e.row_base))
        .collect();
    let mut segments: Vec<(&'static SegmentData, u32)> = view
        .segments
        .iter()
        .map(|s| {
            let base = row_bases.get(s.seg_id.as_str()).copied().unwrap_or(0);
            (s.as_ref(), base)
        })
        .collect();
    segments.sort_unstable_by_key(|&(_, base)| base);
    let locator = RowLocator::new(segments);

    let pack = tessera_store::membership::MembershipPack::open(&prefix.join(Path::new(&layer_pack)))
        .expect("membership pack opens");

    let mut memberships = Vec::new();
    for (ordinal, blob) in pack.iter() {
        if blob.is_empty() {
            continue;
        }
        let Some((record, _)) =
            tessera_lifecycle::membership::decode_record(tessera_types::EntityId::new(1), blob)
        else {
            continue;
        };
        let visible = view.row_space.project_base(&record.members);
        if visible.is_empty() {
            continue;
        }
        memberships.push((ordinal, visible));
    }

    Corpus {
        locator,
        memberships,
    }
}

/// The positions `compute` reads, gathered through the same locator.
pub fn gather(visible: &Bitmap, locator: &RowLocator<'_>) -> Vec<[u32; 2]> {
    visible
        .iter()
        .filter_map(|row| locator.position(row).map(|(x, y)| [x, y]))
        .collect()
}
