//! The merge's four structural invariants, over generated segment shapes rather than chosen ones.
//!
//! `merge_execution.rs` states each of these against a hand-built pair of segments, which is the
//! right form for the *argument* — each case names the failure it exists to catch. What a fixed
//! pair cannot cover is the combinatorial part: how many inputs, how their entity ranges are
//! spaced, how heavily their Morton ranges interleave, and the boundary shapes (a single-row
//! input, a run of them, an input whose points all sort before or after its neighbour's).
//!
//! **Where the coverage actually was thin, and it was not here.** The engine-level merge suite
//! merged four *single-row* extents that were already in Morton order, so its permutation was the
//! identity and every claim about a permuted row space was vacuous; `tests/merge.rs` was rebuilt
//! around an interleaving fixture for that reason. The store-level fixture below never had that
//! problem — `merge_execution::segment` chooses its stride to interleave deliberately — so what
//! this file adds is width, not a missing property.
//!
//! ## The four properties
//!
//! 1. **Row-count preserving.** The output holds exactly as many rows as its inputs did. Dropping
//!    one is the compaction *fold*, which is invariant-bearing work this layer must not do.
//! 2. **Position-preserving, byte-exact.** The `(tessera_id, code, residual)` multiset is
//!    unchanged. A merge that dequantised and requantised — rather than carrying the Morton code
//!    through — moves points on a viewer's map by a sub-cell amount that no count would show.
//! 3. **Morton-sorted output.** The sort *is* the tile index: `tile_ranges` binary-searches it, so
//!    an unsorted segment does not serve wrong counts, it serves nonsense.
//! 4. **The extent is a bijection onto the merged rows**, and every id the inputs did not carry is
//!    `ROW_ABSENT` rather than 0 — mapping a gap to row 0 serves one entity's coordinates under
//!    another's identity, which is the sharpest failure in this list.

mod fixture;

use std::path::Path;

use fixture::{build_bundle, PARTITION, VIEW};
use proptest::prelude::*;
use tessera_spatial::tiler::ScalarType;
use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::Quantisation;
use tessera_store::merge::{execute_merge, MergeInput, MergeSpec};
use tessera_store::read::{ColumnsRef, MortonSlice};
use tessera_types::{EntityId, IdentityKey, TesseraId, ROW_ABSENT};

fn key() -> IdentityKey {
    IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key")
}

fn quantisation() -> Quantisation {
    Quantisation {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// One generated input: where its entity range starts, how many rows, and how hard it interleaves.
///
/// `stride` and `phase` are what vary the *Morton* relationship between inputs independently of
/// the entity relationship. Two inputs can be entity-adjacent and Morton-disjoint (the merge is
/// then a concatenation), entity-adjacent and fully interleaved (every row moves), or anywhere
/// between — and the three are different code paths through the sort even though the entity
/// ranges look identical.
#[derive(Debug, Clone)]
struct Shape {
    gap: u64,
    count: u64,
    stride: u64,
    phase: u64,
}

fn shapes() -> impl Strategy<Value = Vec<Shape>> {
    prop::collection::vec(
        (0u64..40, 1u64..25, 1u64..97, 0u64..89).prop_map(|(gap, count, stride, phase)| Shape {
            gap,
            count,
            stride,
            phase,
        }),
        2..6,
    )
}

/// Write `shape` as a flush segment and return its `MergeInput`, alongside `(entity, code,
/// residual, tessera_id)` for every row it holds.
fn write_input(root: &Path, index: usize, entity_lo: u64, shape: &Shape) -> MergeInput {
    let seg_id = format!("in-{index}");
    let rows: Vec<FlushRow> = (entity_lo..entity_lo + shape.count)
        .map(|e| FlushRow {
            entity_id: EntityId::new(e),
            external_id: Some(format!("ext-{e:012}").into_bytes()),
            x: (((e * shape.stride + shape.phase) % 97) as f64) / 97.0,
            y: (((e * 53 + shape.phase) % 89) as f64) / 89.0,
            scalars: vec![],
        })
        .collect();
    write_flush_segment(
        &root.join("v00000"),
        PARTITION,
        VIEW,
        FlushInput {
            incarnation: 0,
            seg_id: &seg_id,
            rows,
            quantisation: quantisation(),
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &[],
            row_base: 0,
        },
    )
    .expect("the input segment writes");
    MergeInput {
        seg_id,
        entity_lo,
        entity_hi: entity_lo + shape.count - 1,
    }
}

fn seg_dir(root: &Path, seg_id: &str) -> std::path::PathBuf {
    root.join("v00000/partitions")
        .join(PARTITION)
        .join("views")
        .join(VIEW)
        .join("segments")
        .join(seg_id)
}

/// `(tessera_id, code, residual)` for every row of a segment, sorted — the multiset property 2 is
/// about.
fn points_of(root: &Path, seg_id: &str) -> Vec<(u64, u32, u32)> {
    let d = seg_dir(root, seg_id);
    let codes = MortonSlice::load(&d.join("morton.u32")).expect("morton loads");
    let cols = ColumnsRef::load(&d.join("columns.arrow")).expect("columns load");
    let mut out: Vec<(u64, u32, u32)> = (0..codes.u32().len())
        .map(|row| {
            (
                cols.tessera_id()[row],
                codes.u32()[row],
                cols.residual()[row],
            )
        })
        .collect();
    out.sort_unstable();
    out
}

proptest! {
    // Each case writes several segments and merges them, so the cost is file IO rather than
    // arithmetic. 64 is enough to reach the boundary shapes the strategy generates (single-row
    // inputs, zero gaps, Morton-disjoint neighbours) without making the suite a build step.
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn a_merge_preserves_every_row_its_position_and_its_identity(shapes in shapes()) {
        let dir = tempfile::TempDir::new().unwrap();
        build_bundle(dir.path(), 10);

        // Ascending, non-overlapping entity ranges: `execute_merge` requires them (the merged
        // extent is one contiguous span), and `out_of_order_inputs_are_refused` pins the refusal.
        let mut inputs = Vec::new();
        let mut before = Vec::new();
        let mut entity_lo = 100u64;
        for (index, shape) in shapes.iter().enumerate() {
            entity_lo += shape.gap;
            let input = write_input(dir.path(), index, entity_lo, shape);
            before.extend(points_of(dir.path(), &input.seg_id));
            entity_lo += shape.count;
            inputs.push(input);
        }
        before.sort_unstable();

        let schema: Vec<(String, ScalarType)> = vec![];
        let out = execute_merge(
            &dir.path().join("v00000"),
            PARTITION,
            VIEW,
            MergeSpec {
                incarnation: 0,
                seg_id: "merged",
                inputs: &inputs,
                identity_key: &key(),
                shard_id: 0,
                scalar_schema: &schema,
                row_base: 0,
                watermark: 10_000,
                entity_id_high_water: 10_000,
            },
        )
        .expect("the merge executes");

        // **1. row-count preserving.**
        let total: u64 = shapes.iter().map(|s| s.count).sum();
        prop_assert_eq!(u64::from(out.segment.row_count), total);

        // **2. position- and identity-preserving, byte-exact.**
        prop_assert_eq!(points_of(dir.path(), "merged"), before);

        // **3. Morton-sorted.**
        let codes = MortonSlice::load(&seg_dir(dir.path(), "merged").join("morton.u32")).unwrap();
        prop_assert!(codes.u32().windows(2).all(|w| w[0] <= w[1]));

        // **4. the extent is a bijection, and a gap is absent rather than row 0.**
        let cols = ColumnsRef::load(&seg_dir(dir.path(), "merged").join("columns.arrow")).unwrap();
        let base = out.segment.entity_lo;
        let mut seen_rows = Vec::new();
        for input in &inputs {
            for entity in input.entity_lo..=input.entity_hi {
                let row = out.extent.rows[(entity - base) as usize];
                prop_assert_ne!(row, ROW_ABSENT, "entity {} has no row", entity);
                let (shard, back) = key().invert(TesseraId::new(cols.tessera_id()[row as usize]));
                prop_assert_eq!(shard, 0);
                prop_assert_eq!(
                    back.raw(), entity,
                    "the extent's row for {} carries another entity's identity", entity
                );
                seen_rows.push(row);
            }
        }
        seen_rows.sort_unstable();
        seen_rows.dedup();
        prop_assert_eq!(
            seen_rows.len() as u64, total,
            "two entities resolved to one row — the extent is not a bijection"
        );

        // Every id inside the merged span that no input carried must be absent. Row 0 is a real
        // row belonging to a real entity, so a gap mapped there is a cross-identity disclosure.
        let carried: std::collections::BTreeSet<u64> = inputs
            .iter()
            .flat_map(|i| i.entity_lo..=i.entity_hi)
            .collect();
        for entity in base..=out.segment.entity_hi {
            if !carried.contains(&entity) {
                prop_assert_eq!(
                    out.extent.rows[(entity - base) as usize], ROW_ABSENT,
                    "the gap at {} maps to a row it does not own", entity
                );
            }
        }
    }
}
