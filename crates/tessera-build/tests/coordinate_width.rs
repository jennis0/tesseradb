//! The coordinate path's width, at the one place a coordinate is quantised.
//!
//! `projections.md` §6: the path is `f64` from the input file to the quantiser, and **both float
//! widths are accepted on input, the narrower widened**. A whole-world frame is served perfectly
//! well by `f32` coordinates and a corpus emitting them should not have to double the size of its
//! two largest columns to be read; a sub-square frame needs `f64` and the caller supplies it.
//!
//! An `f32` value resolves to about 2^24 steps per axis against a grid of 2^16 cells — 256 steps
//! per cell at the whole-world frame, but only `2^(8−k)` at a sub-square at zoom offset *k*. Past
//! roughly offset 8 there is less than one `f32` step per cell, so a narrowing decides which
//! **cell** a point lands in rather than merely its residual, and no report downstream can see
//! that it did. The frame these tests use is 1/4096 of a 65,536-unit coordinate range — offset 12
//! — where one `f32` step spans sixteen cells.
//!
//! Type signatures prove nothing here, so every case is a pair of positions and the cells they
//! land in.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::input::read_points;
use tessera_spatial::{
    shape::{ShapeF64, Space},
    Bounds,
};

/// A frame 16 units wide out of a 65,536-unit coordinate range — 1/4096 of it, zoom offset 12 —
/// placed at the far end of the range, where an `f32`'s exponent is largest and its step coarsest.
///
/// Both halves matter. A frame this deep makes one cell 16/2^16 ≈ 2.44 × 10⁻⁴ wide; coordinates
/// this large make one `f32` step 2^-8 = 3.9 × 10⁻³, sixteen cells. Near the origin the same
/// frame would be resolved by `f32` perfectly well, which is why the *value* is part of the
/// fixture and not incidental to it.
fn deep_frame() -> Bounds {
    Bounds {
        x_min: 65_504.0,
        x_max: 65_520.0,
        y_min: 65_504.0,
        y_max: 65_520.0,
    }
}

/// Two positions 10⁻³ apart — closer than the 3.9 × 10⁻³ `f32` step at [`deep_frame`], so an
/// `f32` path holds one value where an `f64` path holds two.
const NEAR: (f64, f64) = (65_508.5, 65_508.501);

/// A points file with the coordinate columns at the caller's chosen Arrow float width.
fn write_points(path: &Path, xs: &[f64], ys: &[f64], width: &DataType) {
    let coordinates = |values: &[f64]| -> ArrayRef {
        match width {
            DataType::Float32 => Arc::new(Float32Array::from(
                values.iter().map(|v| *v as f32).collect::<Vec<_>>(),
            )),
            DataType::Float64 => Arc::new(Float64Array::from(values.to_vec())),
            other => panic!("a coordinate column is float32 or float64, not {other:?}"),
        }
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", width.clone(), false),
        Field::new("y", width.clone(), false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from((0..xs.len() as u64).collect::<Vec<_>>())),
            coordinates(xs),
            coordinates(ys),
        ],
    )
    .expect("the fixture batch is well-formed");
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The quantised positions a points file reads to, by source id.
fn positions(path: &Path, extent: &Bounds) -> Vec<(u32, u32)> {
    let mut rows = read_points(
        path,
        &Default::default(),
        tessera_spatial::Projection::None,
        extent,
        None,
            None,
    ).expect("the points read");
    rows.sort_by_key(|r| r.source_id);
    rows.iter().map(|r| (r.qx, r.qy)).collect()
}

/// **A points file holding `float32` still reads, and to the positions it always did.**
///
/// The narrower width is widened rather than refused, and widening an `f32` to `f64` is exact — so
/// every stored position is the same one a build produced when the reader itself was `f32` and
/// cast at the quantiser. Asserted against a `float64` file holding the widened values, which is
/// the same arithmetic stated a second way: if the two ever disagree, the widening is not exact
/// and a corpus emitting single-precision coordinates has silently moved.
#[test]
fn a_float32_points_file_reads_to_the_positions_it_always_did() {
    let tmp = tempfile::tempdir().unwrap();
    let narrow = tmp.path().join("narrow.parquet");
    let wide = tmp.path().join("wide.parquet");
    let extent = deep_frame();

    let xs: Vec<f64> = (0..64).map(|i| 65_504.0 + i as f64 * 0.25).collect();
    let ys: Vec<f64> = (0..64).map(|i| 65_519.75 - i as f64 * 0.25).collect();
    write_points(&narrow, &xs, &ys, &DataType::Float32);
    // The same values the `float32` file actually holds, at the wider width.
    let widened_x: Vec<f64> = xs.iter().map(|v| f64::from(*v as f32)).collect();
    let widened_y: Vec<f64> = ys.iter().map(|v| f64::from(*v as f32)).collect();
    write_points(&wide, &widened_x, &widened_y, &DataType::Float64);

    let narrow_positions = positions(&narrow, &extent);
    assert_eq!(narrow_positions.len(), 64, "every row reads");
    assert_eq!(narrow_positions, positions(&wide, &extent));
}

/// **A `float64` points file keeps a distinction an `f32` path loses.**
///
/// The two positions are closer together than one `f32` step at this frame, so a reader that
/// narrowed them would hold one value; they belong in different cells, four apart. The `f32`
/// arm is asserted rather than described — without it this test passes against a reader that
/// never narrowed anything and proves nothing about the width.
#[test]
fn two_positions_inside_one_f32_step_land_in_different_cells() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("points.parquet");
    let extent = deep_frame();
    let (a, b) = NEAR;

    assert!(
        b - a < f64::from(f32::EPSILON) * a,
        "the fixture pair must be closer than one f32 step, or this test discriminates nothing"
    );
    assert_eq!(
        a as f32, b as f32,
        "the fixture pair must collapse under f32, which is the loss being ruled out"
    );

    write_points(&path, &[a, b], &[a, b], &DataType::Float64);
    let got = positions(&path, &extent);
    assert_eq!(got.len(), 2);
    assert_ne!(
        (got[0].0 >> 16, got[0].1 >> 16),
        (got[1].0 >> 16, got[1].1 >> 16),
        "the two positions must occupy different cells: {got:?}"
    );
}

/// **A point inside a small polygon at its source coordinates is inside it at its stored
/// position** — the asymmetry between the shape path and the point path, as a test.
///
/// `tessera_spatial::shape` quantises a shape's vertices through the same `fixed32` at the full 32
/// bits per axis, so a polygon a fraction of a cell wide survives with room to spare: the square
/// here is 8 × 10⁻⁴ units across, which is 52 grid steps at the whole-world frame. A coordinate
/// read at `f32` moves by up to half of that frame's 3.9 × 10⁻³ step — an order of magnitude
/// further than the whole polygon is wide — so the point lands outside a shape that holds it.
///
/// This is not a hypothetical: of 17,551 Overture divisions, three metres-wide polygons hold a
/// place that is inside at its source coordinates and outside at its stored position.
///
/// The frame here is the **whole-world** one rather than [`deep_frame`], because the shape path
/// makes the asymmetry visible without any depth at all: a shape resolves 2^32 steps per axis
/// where a narrowed point resolves 2^24.
#[test]
fn a_point_inside_a_small_polygon_is_inside_it_at_its_stored_position() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("points.parquet");
    let extent = Bounds {
        x_min: 0.0,
        x_max: 65_536.0,
        y_min: 0.0,
        y_max: 65_536.0,
    };

    // A square 8 × 10⁻⁴ units across, centred on the point.
    let (cx, cy) = (65_500.501, 65_500.501);
    let (lo, hi) = (0.000_4_f64, 0.000_4_f64);
    let square = ShapeF64::Polygon(vec![vec![vec![
        (cx - lo, cy - lo),
        (cx + hi, cy - lo),
        (cx + hi, cy + hi),
        (cx - lo, cy + hi),
        (cx - lo, cy - lo),
    ]]]);
    let (shape, report) = square.canonical(Space::View, &extent).expect("the fixture square canonicalises");
    assert_eq!(
        report.rings_dropped, 0,
        "the square must survive quantisation, or nothing below is being tested"
    );

    write_points(&path, &[cx], &[cy], &DataType::Float64);
    let stored = positions(&path, &extent)[0];
    assert!(
        shape.contains(stored),
        "the stored position {stored:?} fell outside a polygon its source coordinates are inside"
    );

    // And the position an `f32` path would have stored is outside it — which is what makes the
    // assertion above a property of the width rather than of the fixture.
    let narrowed = (
        tessera_spatial::fixed32(f64::from(cx as f32), extent.x_min, extent.x_max),
        tessera_spatial::fixed32(f64::from(cy as f32), extent.y_min, extent.y_max),
    );
    assert!(
        !shape.contains(narrowed),
        "the fixture polygon must be small enough that an f32 position misses it"
    );
}
