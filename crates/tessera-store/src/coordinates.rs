//! A view's coordinate columns: what they are called, how one is read, and where each row lands in
//! the view's frame. A build reads a points file here and a running service reads an ingest batch
//! here, so one row is placed the same way at either.

use std::fmt;

use arrow::array::Array;
use arrow::datatypes::DataType;
use tessera_spatial::{Bounds, Projection};

use crate::scalar_column::f64_values;

/// The columns a view's coordinates are read from: `x` and `y` where it projects nothing, and
/// `lon` and `lat` where it projects the Earth. A build renames them through a declaration's
/// `fields`.
pub fn axis_names(projection: Projection) -> (&'static str, &'static str) {
    match projection {
        Projection::None => ("x", "y"),
        _ => ("lon", "lat"),
    }
}

/// The other kind of view's name for each axis, paired with this view's name for it.
pub fn other_axis_names(projection: Projection) -> [(&'static str, &'static str); 2] {
    let (x, y) = axis_names(projection);
    let (other_x, other_y) = match projection {
        Projection::None => axis_names(Projection::PLATE_CARREE),
        _ => axis_names(Projection::None),
    };
    [(other_x, x), (other_y, y)]
}

/// An axis a batch names for the other kind of view while leaving this view's name for it out, as
/// `(wrong, right)`. A batch has no `fields` to rename its columns through, so a running service
/// refuses one rather than mirroring or misreading the corpus.
pub fn misnamed_axis(
    projection: Projection,
    has_column: impl Fn(&str) -> bool,
) -> Option<(&'static str, &'static str)> {
    other_axis_names(projection)
        .into_iter()
        .find(|(wrong, right)| has_column(wrong) && !has_column(right))
}

/// Why a coordinate column cannot be read.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnError {
    /// The column is of a type other than `float32` or `float64`.
    Type(DataType),
    /// Row `row` of the column is null.
    Null { row: usize },
}

impl fmt::Display for ColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnError::Type(found) => write!(f, "is {found:?}; send float32 or float64"),
            ColumnError::Null { .. } => {
                write!(f, "is null; write a number, since every row needs both coordinates")
            }
        }
    }
}

/// A coordinate column as `f64`, by [`f64_values`]: `float32` is widened and `float64` kept as it
/// is, since at a deep frame narrowing would move a point into another cell.
pub fn read_coordinates(column: &dyn Array) -> Result<Vec<f64>, ColumnError> {
    read_optional_coordinates(column)?
        .into_iter()
        .enumerate()
        .map(|(row, value)| value.ok_or(ColumnError::Null { row }))
        .collect()
}

/// [`read_coordinates`], with `None` for a null row: an ingest row may carry no position.
pub fn read_optional_coordinates(column: &dyn Array) -> Result<Vec<Option<f64>>, ColumnError> {
    let values =
        f64_values(column).ok_or_else(|| ColumnError::Type(column.data_type().clone()))?;
    // A null slot's value buffer holds an arbitrary number, so it is never read.
    Ok(values
        .into_iter()
        .enumerate()
        .map(|(row, value)| (!column.is_null(row)).then_some(value))
        .collect())
}

/// Where one row lands in a view's frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    /// The row's position after the projection and before the extent's clamp.
    pub projected: (f64, f64),
    /// The position stored: [`Self::projected`], moved onto the extent's edge on each axis it
    /// lies outside.
    pub x: f64,
    pub y: f64,
    /// The latitude lay beyond the projection's domain, so the projection moved it onto the
    /// domain's edge.
    pub clipped: bool,
    /// The projected position lay outside the extent on x, and on y. An extent holds its maxima.
    pub clamped_x: bool,
    pub clamped_y: bool,
}

impl Placed {
    /// Whether the row was moved onto the extent's edge on either axis.
    pub fn clamped(&self) -> bool {
        self.clamped_x || self.clamped_y
    }
}

/// Why a row has no place in a view's frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unplaceable {
    NonFinite { x: f64, y: f64 },
    /// A projected view's row outside WGS84's range.
    NotWgs84 {
        lon: f64,
        lat: f64,
        projection: Projection,
    },
}

impl fmt::Display for Unplaceable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unplaceable::NonFinite { x, y } => {
                write!(f, "is at ({x}, {y}), which is not a position; send finite numbers")
            }
            Unplaceable::NotWgs84 {
                lon,
                lat,
                projection,
            } => write!(
                f,
                "is at lon {lon}, lat {lat}, which is not a place; this view is projected ({}), \
                 so send WGS84 degrees with longitude within ±180 and latitude within ±90",
                projection.name()
            ),
        }
    }
}

/// Place one row read from a view's coordinate columns. The projection runs first, and then the
/// row is clamped onto `extent` where one is given. A non-finite coordinate is refused, and under
/// a projection so is one outside WGS84's range.
pub fn place(
    projection: Projection,
    extent: Option<&Bounds>,
    x: f64,
    y: f64,
) -> Result<Placed, Unplaceable> {
    if !x.is_finite() || !y.is_finite() {
        return Err(Unplaceable::NonFinite { x, y });
    }
    let mut clipped = false;
    if projection != Projection::None {
        if x.abs() > 180.0 || y.abs() > 90.0 {
            return Err(Unplaceable::NotWgs84 {
                lon: x,
                lat: y,
                projection,
            });
        }
        // Tested before the transform, which moves a clipped row onto the extent's edge where no
        // clamp is counted.
        clipped = projection.is_clipped(y);
    }
    let projected = projection.forward(x, y);
    let (px, py) = projected;
    let placed = match extent {
        None => Placed {
            projected,
            x: px,
            y: py,
            clipped,
            clamped_x: false,
            clamped_y: false,
        },
        Some(e) => Placed {
            projected,
            x: px.clamp(e.x_min, e.x_max),
            y: py.clamp(e.y_min, e.y_max),
            clipped,
            clamped_x: px < e.x_min || px > e.x_max,
            clamped_y: py < e.y_min || py > e.y_max,
        },
    };
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float32Array, Float64Array, Int32Array};

    #[test]
    fn a_float32_column_is_widened_and_a_float64_one_kept() {
        let narrow = Float32Array::from(vec![0.1f32, -2.5]);
        assert_eq!(
            read_coordinates(&narrow),
            Ok(vec![f64::from(0.1f32), -2.5])
        );
        let wide = Float64Array::from(vec![1_000.000_02f64]);
        assert_eq!(read_coordinates(&wide), Ok(vec![1_000.000_02]));
    }

    #[test]
    fn any_other_type_is_refused() {
        let ints = Int32Array::from(vec![1]);
        assert_eq!(read_coordinates(&ints), Err(ColumnError::Type(DataType::Int32)));
    }

    #[test]
    fn a_null_is_refused_naming_its_row() {
        let column = Float64Array::from(vec![Some(1.0), Some(2.0), None, None]);
        assert_eq!(read_coordinates(&column), Err(ColumnError::Null { row: 2 }));
        let column = Float32Array::from(vec![None, Some(1.0f32)]);
        assert_eq!(read_coordinates(&column), Err(ColumnError::Null { row: 0 }));
    }

    const EXTENT: Bounds = Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    };

    #[test]
    fn a_projected_view_reads_lon_and_lat_and_refuses_the_other_spelling() {
        assert_eq!(axis_names(Projection::None), ("x", "y"));
        assert_eq!(axis_names(Projection::WebMercator), ("lon", "lat"));
        let columns = ["x", "y"];
        let has = |name: &str| columns.contains(&name);
        assert_eq!(misnamed_axis(Projection::WebMercator, has), Some(("x", "lon")));
        assert_eq!(misnamed_axis(Projection::None, has), None);
        let columns = ["lon", "lat", "x"];
        let has = |name: &str| columns.contains(&name);
        assert_eq!(misnamed_axis(Projection::None, has), Some(("lat", "y")));
        assert_eq!(misnamed_axis(Projection::GALL_ISOGRAPHIC, has), None);
    }

    #[test]
    fn a_row_outside_the_extent_is_clamped_onto_its_edge_and_its_maximum_is_inside() {
        let placed = place(Projection::None, Some(&EXTENT), 5000.0, -3.0).unwrap();
        assert_eq!((placed.x, placed.y), (1000.0, 0.0));
        assert_eq!(placed.projected, (5000.0, -3.0));
        assert!(placed.clamped_x && placed.clamped_y && placed.clamped());
        let placed = place(Projection::None, Some(&EXTENT), 1000.0, 0.0).unwrap();
        assert!(!placed.clamped());
        let placed = place(Projection::None, None, 5000.0, -3.0).unwrap();
        assert_eq!((placed.x, placed.y), (5000.0, -3.0));
        assert!(!placed.clamped());
    }

    #[test]
    fn a_non_finite_row_is_refused_under_any_projection() {
        for projection in [Projection::None, Projection::WebMercator] {
            for (x, y) in [(f64::NAN, 1.0), (1.0, f64::INFINITY)] {
                assert!(matches!(
                    place(projection, Some(&EXTENT), x, y),
                    Err(Unplaceable::NonFinite { .. })
                ));
            }
        }
    }

    #[test]
    fn a_projected_row_outside_wgs84_is_refused_and_one_past_the_domain_is_clipped() {
        let frame = Bounds {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        };
        for (lon, lat) in [(180.5, 0.0), (0.0, -90.5)] {
            assert!(matches!(
                place(Projection::WebMercator, Some(&frame), lon, lat),
                Err(Unplaceable::NotWgs84 { .. })
            ));
        }
        let polar = place(Projection::WebMercator, Some(&frame), 10.0, 89.5).unwrap();
        assert!(polar.clipped && !polar.clamped());
        assert_eq!(polar.y, 0.0);
        let inside = place(Projection::PLATE_CARREE, Some(&frame), 180.0, 90.0).unwrap();
        assert!(!inside.clipped && !inside.clamped());
        assert_eq!((inside.x, inside.y), (1.0, 0.0));
    }
}
