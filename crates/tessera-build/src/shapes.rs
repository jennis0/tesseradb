//! **A shape layer's geometry, read and canonicalised at the build** (`polygon-membership.md`
//! §4.2–§4.4, §6.1, §6.5) — and the report `tessera check` computes from the geometry alone,
//! before any build.
//!
//! One reader for the table and the inline spellings, because the two are one thing in two forms:
//! a row's geometry sits in its layer's kind's fields — a box's four bounds, a circle's three, an
//! ellipse's five, a polygon's WKB `geometry` column in a table or `wkt` inline — and leaves here
//! as the canonical grid-unit bytes the record carries, one per view. What canonicalisation did is
//! **reported and never refused**: a clipped boundary, a ring that collapsed, a table that looks
//! written in degrees for a view that is not. What refuses is a coordinate that is not one, a
//! non-positive radius or axis, a polygon over the vertex cap, and a row whose geometry is another
//! kind's — each the caller's own arithmetic with a one-line fix.
//!
//! **The decomposition is corpus-independent, so its size is known before any memory is committed
//! to it.** Every shape is decomposed here as it is canonicalised, and the interior-tile and
//! boundary-cell counts go into the report beside the layer — total and the maximum any one
//! artifact holds — which is the number §9's held-size model needs and nobody can estimate from a
//! vertex count.

use std::collections::BTreeMap;
use std::path::Path;

use arrow::array::{Array, BinaryArray, Float64Array, LargeBinaryArray, StringArray};
use arrow::record_batch::RecordBatch;
use tessera_lifecycle::membership::ArtifactShapes;
use tessera_spatial::Bounds;
use tessera_store::derived::{
    canonical_shapes, shape_input, ShapeInput, ShapeSpace, ShapeStats,
};
use tessera_types::layer::{
    LayerDeclaration, MembershipSource, ShapeKind, DEFAULT_MAX_SHAPE_VERTICES,
};

use crate::config::{ArtifactSource, Config, Extent, Fields, InlineArtifact};
use crate::error::{BuildError, Result};

/// The frame a layer's shapes are canonicalised in: the extent the points are quantised against,
/// and the views the layer is drawn in.
#[derive(Debug, Clone)]
pub struct ShapeContext {
    pub extent: Bounds,
    pub views: Vec<String>,
    /// The publication vertex cap (`polygon-membership.md` §9, ruling (e)).
    pub max_vertices: u64,
}

/// What a shape layer's geometry is, before any build (`polygon-membership.md` §6.5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShapeLayerReport {
    pub layer: String,
    pub kind: String,
    pub views: Vec<String>,
    /// Artifacts carrying a shape.
    pub artifacts: u64,
    /// Rows with no geometry — published with an empty shape and no members.
    pub no_geometry: u64,
    pub parts: u64,
    pub rings: u64,
    pub vertices_in: u64,
    pub vertices_out: u64,
    pub clipped: u64,
    pub outside: u64,
    pub rings_dropped: u64,
    /// Shapes whose every coordinate lay within ±180 × ±90 on a view whose extent does not: the
    /// R12 report, a table that may have been written in degrees for a view that is not.
    pub degrees_looking: u64,
    /// Children whose canonical bounds escape their declared parent's — reported, never checked
    /// against the edges (§6.2).
    pub children_escaping: u64,
    pub interior_tiles: u64,
    pub boundary_cells: u64,
    pub max_interior_tiles: u64,
    pub max_boundary_cells: u64,
    /// The canonical bytes the records carry, summed over views.
    pub canonical_bytes: u64,
    /// The held decomposition's bytes beyond the shapes' own — sixteen per interior tile, five
    /// per boundary cell (§9).
    pub held_bytes: u64,
    /// The layer spans several views, none of which declares a projection, so nothing says whether
    /// they share a space (§4.3) — warned, never refused.
    pub several_views: bool,
    /// The resolution's cost, filled by the build's artifact pass and absent from a check.
    pub resolution: Option<ResolutionReport>,
}

/// What resolving the build's segment against a layer's shapes cost (§9's per-flush row, measured
/// at the build over its one segment).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolutionReport {
    pub rows: u64,
    pub rows_tested: u64,
    pub rows_interior: u64,
    /// Artifacts holding a shape and no row of the segment. The layout line counts artifacts
    /// **with rows**, the geometry line counts artifacts **with a shape**, and this is the
    /// difference, stated.
    pub artifacts_empty: u64,
    /// The keys of the first few of those, so the count can be looked into rather than trusted.
    pub empty_keys: Vec<String>,
    pub elapsed_ms: u64,
}

impl ShapeLayerReport {
    /// One block of the build's or the check's report, on stderr like the rest of them.
    pub fn print(&self) {
        eprintln!(
            "  {} [{}]: {} artifact(s) with a shape, {} without geometry; {} part(s), {} ring(s); \
             vertices {} in → {} out",
            self.layer,
            self.kind,
            self.artifacts,
            self.no_geometry,
            self.parts,
            self.rings,
            self.vertices_in,
            self.vertices_out
        );
        eprintln!(
            "    clipped to the extent {}, wholly outside {}, rings dropped {}, degrees-looking \
             {}, children escaping their parent's bounds {}",
            self.clipped, self.outside, self.rings_dropped, self.degrees_looking, self.children_escaping
        );
        eprintln!(
            "    decomposition: {} interior tile(s) (max {} per artifact), {} boundary cell(s) \
             (max {} per artifact); canonical {} B, held {} B",
            self.interior_tiles,
            self.max_interior_tiles,
            self.boundary_cells,
            self.max_boundary_cells,
            self.canonical_bytes,
            self.held_bytes
        );
        if self.several_views {
            eprintln!(
                "    WARNING: the layer is drawn in {} views ({}) and none declares a projection, \
                 so nothing says whether they share a coordinate system; the shapes are resolved \
                 in each (`polygon-membership.md` §4.3)",
                self.views.len(),
                self.views.join(", ")
            );
        }
        if let Some(r) = &self.resolution {
            eprintln!(
                "    resolved the build's segment: {} row(s), {} admitted from interior tiles, {} \
                 tested one by one in boundary cells, {} artifact(s) with a shape and no row, {} ms",
                r.rows, r.rows_interior, r.rows_tested, r.artifacts_empty, r.elapsed_ms
            );
        }
    }
}

/// The geometry columns of one artifact table, in the layer's kind's fields.
pub struct ShapeColumns<'a> {
    kind: ShapeKind,
    f64s: Vec<Option<&'a Float64Array>>,
    wkb: Option<Wkb<'a>>,
    space: Option<&'a StringArray>,
}

enum Wkb<'a> {
    Binary(&'a BinaryArray),
    Large(&'a LargeBinaryArray),
}

impl<'a> ShapeColumns<'a> {
    /// The kind's columns, under the names `fields` resolved. A column the kind reads and the file
    /// lacks is an absence for every row rather than a refusal: a row with no geometry is
    /// published and reported (§6.1).
    pub fn open(
        path: &Path,
        batch: &'a RecordBatch,
        fields: &Fields,
        kind: ShapeKind,
    ) -> Result<Self> {
        let names: &[&str] = match kind {
            ShapeKind::Bbox => &["min_x", "min_y", "max_x", "max_y"],
            ShapeKind::Circle => &["cx", "cy", "r"],
            ShapeKind::Ellipse => &["cx", "cy", "a", "b", "angle"],
            ShapeKind::Polygon => &[],
        };
        let mut f64s = Vec::with_capacity(names.len());
        for name in names {
            f64s.push(optional_f64(path, batch, fields, name)?);
        }
        let wkb = if kind == ShapeKind::Polygon {
            match optional(path, batch, fields, "geometry")? {
                None => None,
                Some(array) => Some(match array.data_type() {
                    arrow::datatypes::DataType::Binary => Wkb::Binary(typed(path, array, "geometry")?),
                    arrow::datatypes::DataType::LargeBinary => {
                        Wkb::Large(typed(path, array, "geometry")?)
                    }
                    other => {
                        return Err(BuildError::Invalid(format!(
                            "{}: column '{}' is {other:?}; a polygon's `geometry` column is WKB, \
                             which is Binary or LargeBinary (GeoParquet's own encoding)",
                            path.display(),
                            fields.of("geometry")
                        )))
                    }
                }),
            }
        } else {
            None
        };
        let space = match optional(path, batch, fields, "space")? {
            None => None,
            Some(array) => Some(typed::<StringArray>(path, array, fields.of("space"))?),
        };
        Ok(ShapeColumns {
            kind,
            f64s,
            wkb,
            space,
        })
    }

    /// The row's geometry as declared, `None` where the row carries none.
    ///
    /// **All of a kind's numbers or none**: a box assembled from two present bounds and two
    /// defaults is a region nobody wrote, and on a shape layer that region is the membership.
    pub fn at(&self, path: &Path, row: usize, key: &str) -> Result<Option<ShapeInput>> {
        if self.kind == ShapeKind::Polygon {
            let bytes = match &self.wkb {
                None => None,
                Some(Wkb::Binary(a)) => (!a.is_null(row)).then(|| a.value(row).to_vec()),
                Some(Wkb::Large(a)) => (!a.is_null(row)).then(|| a.value(row).to_vec()),
            };
            return Ok(bytes.map(ShapeInput::Wkb));
        }
        let present: Vec<Option<f64>> = self
            .f64s
            .iter()
            .map(|column| column.filter(|c| !c.is_null(row)).map(|c| c.value(row)))
            .collect();
        if present.iter().all(Option::is_none) {
            return Ok(None);
        }
        if present.iter().any(Option::is_none) {
            return Err(BuildError::Invalid(format!(
                "{}: artifact {key} declares some of its {}'s fields and not all of them. A shape \
                 assembled from the ones that are there is a region nobody wrote, and on a layer \
                 whose `shape` declares one that region is the membership",
                path.display(),
                self.kind.as_str()
            )));
        }
        let v: Vec<f64> = present.into_iter().flatten().collect();
        Ok(Some(match self.kind {
            ShapeKind::Bbox => ShapeInput::Bbox([v[0], v[1], v[2], v[3]]),
            ShapeKind::Circle => ShapeInput::Circle([v[0], v[1], v[2]]),
            ShapeKind::Ellipse => ShapeInput::Ellipse([v[0], v[1], v[2], v[3], v[4]]),
            ShapeKind::Polygon => unreachable!("handled above"),
        }))
    }

    /// The row's own `space`, where the table carries the column.
    pub fn space_at(&self, row: usize) -> Option<String> {
        self.space
            .filter(|c| !c.is_null(row))
            .map(|c| c.value(row).to_string())
    }
}

/// An inline row's geometry, in its layer's kind's field.
pub fn inline_shape(row: &InlineArtifact, kind: ShapeKind) -> Result<Option<ShapeInput>> {
    let mut carried: Vec<(&str, ShapeInput)> = Vec::new();
    let refuse_count = |field: &str, n: usize, want: usize| {
        BuildError::Invalid(format!(
            "artifact {}: `{field}` has {n} value(s); it is exactly {want}",
            row.key
        ))
    };
    if let Some(v) = &row.bbox {
        let [a, b, c, d] = v[..] else {
            return Err(refuse_count("bbox", v.len(), 4));
        };
        carried.push(("bbox", ShapeInput::Bbox([a, b, c, d])));
    }
    if let Some(v) = &row.circle {
        let [a, b, c] = v[..] else {
            return Err(refuse_count("circle", v.len(), 3));
        };
        carried.push(("circle", ShapeInput::Circle([a, b, c])));
    }
    if let Some(v) = &row.ellipse {
        let [a, b, c, d, e] = v[..] else {
            return Err(refuse_count("ellipse", v.len(), 5));
        };
        carried.push(("ellipse", ShapeInput::Ellipse([a, b, c, d, e])));
    }
    if let Some(text) = &row.wkt {
        carried.push(("wkt", ShapeInput::Wkt(text.clone())));
    }
    match carried.len() {
        0 => Ok(None),
        1 => {
            let (_, input) = carried.pop().expect("one");
            if input.kind() != kind {
                return Err(BuildError::Invalid(format!(
                    "artifact {}: carries a {} and the layer's `shape.kind` is \"{}\"; a row's \
                     geometry is in its layer's kind's field and no other",
                    row.key,
                    input.kind().as_str(),
                    kind.as_str()
                )));
            }
            Ok(Some(input))
        }
        _ => Err(BuildError::Invalid(format!(
            "artifact {}: carries {} — one row has one shape, in its layer's kind's field",
            row.key,
            carried
                .iter()
                .map(|(f, _)| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(" and ")
        ))),
    }
}

/// Canonicalises one layer's shapes as its rows are read, accumulating the report.
pub struct ShapeReader {
    kind: ShapeKind,
    ctx: ShapeContext,
    default_space: ShapeSpace,
    report: ShapeLayerReport,
    /// Per key, the grid-unit bounds per view — for the parent-escape report.
    bounds: BTreeMap<String, Vec<(String, Option<tessera_spatial::shape::Bbox>)>>,
}

impl ShapeReader {
    pub fn new(
        layer: &str,
        kind: ShapeKind,
        ctx: ShapeContext,
        default_space: ShapeSpace,
    ) -> Self {
        let several_views = ctx.views.len() > 1;
        ShapeReader {
            kind,
            report: ShapeLayerReport {
                layer: layer.to_string(),
                kind: kind.as_str().to_string(),
                views: ctx.views.clone(),
                several_views,
                ..Default::default()
            },
            ctx,
            default_space,
            bounds: BTreeMap::new(),
        }
    }

    pub fn kind(&self) -> ShapeKind {
        self.kind
    }

    /// One row's geometry, canonicalised for every view of the layer.
    ///
    /// A row with no geometry is **published with an empty shape** — an artifact with no members,
    /// a state the service already has — and counted; a row whose `space` the view cannot honour
    /// is refused naming the row.
    pub fn row(
        &mut self,
        key: &str,
        input: Option<ShapeInput>,
        space: Option<&str>,
    ) -> Result<Option<ArtifactShapes>> {
        let _space = match space {
            None => self.default_space,
            Some(word) => ShapeSpace::parse(word).map_err(|e| {
                BuildError::Invalid(format!(
                    "layer '{}': artifact {key}: `space`: {e}",
                    self.report.layer
                ))
            })?,
        };
        let shape = match input {
            None => {
                self.report.no_geometry += 1;
                tessera_spatial::shape::ShapeF64::Polygon(Vec::new())
            }
            Some(input) => shape_input(self.kind, input).map_err(|e| {
                BuildError::Invalid(format!(
                    "layer '{}': artifact {key}: {e}",
                    self.report.layer
                ))
            })?,
        };
        let views: Vec<&str> = self.ctx.views.iter().map(String::as_str).collect();
        let canonical = canonical_shapes(&shape, &views, &self.ctx.extent, self.ctx.max_vertices)
            .map_err(|e| {
                BuildError::Invalid(format!(
                    "layer '{}': artifact {key}: {e}",
                    self.report.layer
                ))
            })?;
        self.report.artifacts += 1;
        // The report reads the first view's canonicalisation, every view sharing one frame today.
        if let Some((_, report, stats)) = canonical.reports.first() {
            self.note(report, stats);
        }
        self.report.canonical_bytes += canonical
            .by_view
            .iter()
            .map(|(v, b)| (v.len() + b.len()) as u64)
            .sum::<u64>();
        self.bounds.insert(key.to_string(), canonical.bounds.clone());
        Ok(ArtifactShapes::new(canonical.by_view))
    }

    fn note(&mut self, report: &tessera_spatial::shape::CanonReport, stats: &ShapeStats) {
        let r = &mut self.report;
        r.parts += stats.parts;
        r.rings += stats.rings;
        r.vertices_in += report.vertices_in;
        r.vertices_out += report.vertices_out;
        r.clipped += u64::from(report.clipped);
        r.outside += u64::from(report.outside);
        r.rings_dropped += u64::from(report.rings_dropped);
        r.degrees_looking += u64::from(report.degrees_looking);
        r.interior_tiles += stats.interior_tiles;
        r.boundary_cells += stats.boundary_cells;
        r.max_interior_tiles = r.max_interior_tiles.max(stats.interior_tiles);
        r.max_boundary_cells = r.max_boundary_cells.max(stats.boundary_cells);
        r.held_bytes += 16 * stats.interior_tiles + 5 * stats.boundary_cells;
    }

    /// The report, with the parent-escape count over the declared edges: a child whose canonical
    /// bounds reach outside its parent's, in any view. **Reported, never checked against the
    /// geometry** — Overture's polygons are generalised for cartography and do not nest reliably,
    /// and a service that derived the tree from containment would build a different tree from the
    /// publisher's (`polygon-membership.md` §6.2).
    pub fn finish(mut self, parents: impl IntoIterator<Item = (String, String)>) -> ShapeLayerReport {
        for (child, parent) in parents {
            let (Some(child), Some(parent)) = (self.bounds.get(&child), self.bounds.get(&parent))
            else {
                continue;
            };
            let escapes = child.iter().zip(parent).any(|((_, c), (_, p))| match (c, p) {
                (Some(c), Some(p)) => {
                    c.min_x < p.min_x || c.min_y < p.min_y || c.max_x > p.max_x || c.max_y > p.max_y
                }
                (Some(_), None) => true,
                _ => false,
            });
            if escapes {
                self.report.children_escaping += 1;
            }
        }
        self.report
    }
}

/// The shape kind, default space and views of a layer that declares a shape, or `None`.
pub fn shape_declared(declaration: &LayerDeclaration) -> Option<ShapeKind> {
    (declaration.membership == MembershipSource::Spatial)
        .then_some(declaration.shape.map(|s| s.kind))
        .flatten()
}

/// **`tessera check`'s shape report, from the geometry alone** (`polygon-membership.md` §6.5):
/// every shape layer's rows read and canonicalised against its view's declared extent, and the
/// decomposition sized, before any build. A view whose extent is `auto` has no frame until the
/// points are read, and such a layer is reported as unsized rather than guessed at.
pub fn check_reports(config: &Config) -> Vec<std::result::Result<ShapeLayerReport, String>> {
    let mut out = Vec::new();
    for sources in &config.layer_sources {
        let Some(declaration) = config.layers.iter().find(|d| d.name == sources.name) else {
            continue;
        };
        let Some(kind) = shape_declared(declaration) else {
            continue;
        };
        let extent = declaration
            .views
            .first()
            .and_then(|name| config.views.iter().find(|v| &v.name == name))
            .map(|view| match &view.extent {
                Extent::Fixed(bounds) => Ok(*bounds),
                Extent::Auto { .. } => Err(format!(
                    "layer '{}': its view's extent is `auto`, which is fitted to the points at the \
                     build; the shapes cannot be sized before then. Declare the extent to size \
                     them here",
                    declaration.name
                )),
            });
        let extent = match extent {
            Some(Ok(extent)) => extent,
            Some(Err(why)) => {
                out.push(Err(why));
                continue;
            }
            None => continue,
        };
        let ctx = ShapeContext {
            extent,
            views: declaration.views.clone(),
            max_vertices: DEFAULT_MAX_SHAPE_VERTICES,
        };
        let result = (|| -> Result<ShapeLayerReport> {
            let mut parents: Vec<(String, String)> = Vec::new();
            match &sources.artifacts {
                None => Ok(ShapeReader::new(&declaration.name, kind, ctx, ShapeSpace::View)
                    .finish(parents)),
                Some(ArtifactSource::Inline(rows)) => {
                    let mut reader =
                        ShapeReader::new(&declaration.name, kind, ctx, ShapeSpace::View);
                    for row in rows {
                        reader.row(&row.key, inline_shape(row, kind)?, row.space.as_deref())?;
                        if let Some(parent) = &row.parent {
                            parents.push((row.key.clone(), parent.clone()));
                        }
                    }
                    Ok(reader.finish(parents))
                }
                Some(ArtifactSource::File {
                    path,
                    fields,
                    default_space,
                }) => {
                    let mut reader =
                        ShapeReader::new(&declaration.name, kind, ctx, *default_space);
                    for batch in crate::layers::batches(path)? {
                        let batch = batch?;
                        let keys = crate::layers::key_column(path, &batch, fields, "key")?;
                        let parent = optional_utf8(path, &batch, fields, "parent")?;
                        let columns = ShapeColumns::open(path, &batch, fields, kind)?;
                        for row in 0..batch.num_rows() {
                            let key = crate::layers::key_at(&keys, row);
                            let space = columns.space_at(row);
                            reader.row(&key, columns.at(path, row, &key)?, space.as_deref())?;
                            if let Some(parent) = parent.and_then(|c| {
                                (!c.is_null(row)).then(|| c.value(row).to_string())
                            }) {
                                parents.push((key, parent));
                            }
                        }
                    }
                    Ok(reader.finish(parents))
                }
            }
        })();
        out.push(result.map_err(|e| e.to_string()));
    }
    out
}

// ---- the column helpers, on `layers.rs`'s own rules ----------------------------------------

fn optional<'a>(
    path: &Path,
    batch: &'a RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a std::sync::Arc<dyn Array>>> {
    let name = fields.of(canonical);
    match batch.column_by_name(name) {
        Some(array) => Ok(Some(array)),
        None if fields.names(canonical) => Err(BuildError::Invalid(format!(
            "{}: {} reads field `{canonical}` from a column named '{name}', which this file does \
             not carry",
            path.display(),
            fields.object()
        ))),
        None => Ok(None),
    }
}

fn typed<'a, T: 'static>(
    path: &Path,
    array: &'a std::sync::Arc<dyn Array>,
    name: &str,
) -> Result<&'a T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        BuildError::Invalid(format!(
            "{}: column {name} is {:?}, which this reader cannot take",
            path.display(),
            array.data_type()
        ))
    })
}

fn optional_f64<'a>(
    path: &Path,
    batch: &'a RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a Float64Array>> {
    match optional(path, batch, fields, canonical)? {
        None => Ok(None),
        Some(array) => typed(path, array, fields.of(canonical)).map(Some),
    }
}

fn optional_utf8<'a>(
    path: &Path,
    batch: &'a RecordBatch,
    fields: &Fields,
    canonical: &str,
) -> Result<Option<&'a StringArray>> {
    match optional(path, batch, fields, canonical)? {
        None => Ok(None),
        Some(array) => typed(path, array, fields.of(canonical)).map(Some),
    }
}
