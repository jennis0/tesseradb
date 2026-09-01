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
use tessera_store::derived::{
    canonical_shapes, check_shape_span, shape_input, ShapeInput, ShapeSpace, ShapeStats, ViewFrame,
};
use tessera_types::layer::{
    LayerDeclaration, MembershipSource, ShapeKind, DEFAULT_MAX_SHAPE_VERTICES,
};

use crate::config::{ArtifactSource, Config, Extent, Fields, InlineArtifact};
use crate::error::{BuildError, Result};

/// The frame a layer's shapes are canonicalised in: the extent the points are quantised against,
/// the transform that placed them there, and the views the layer is drawn in.
#[derive(Debug, Clone)]
pub struct ShapeContext {
    /// **One frame per view, and never one for the layer** (decision 0111): each view's own
    /// declared projection — what a `wgs84` shape is put through, the same function that view's
    /// points went through — beside the extent that view quantises against. A layer's views need
    /// share neither, and a group's share both by construction.
    pub views: Vec<ViewFrame>,
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
    /// The four counts below are the **caller's own geometry, counted once per shape** however
    /// many views the layer is drawn on: a layer over three views holds one polygon, not three
    /// (decision 0111). `vertices_out` is the first view's canonical count, the frames being able
    /// to clip differently.
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
    /// Per view, what canonicalisation did in **that view's own frame** — the numbers the
    /// out-of-extent warning is made of, which a sum over views cannot say (decision 0111). One
    /// entry per view of the layer, in the layer's declared order.
    pub by_view: Vec<ShapeViewReport>,
    /// The resolution's cost, filled by the build's artifact pass and absent from a check.
    pub resolution: Option<ResolutionReport>,
}

/// One view's own half of a [`ShapeLayerReport`]: what canonicalising this layer's shapes against
/// **that view's** extent and projection did.
///
/// **Where the frames differ these differ**, which is the whole reason they are not summed: a
/// boundary inside one view's extent and wholly outside another's is exactly the state the operator
/// has to see, and a total of `1` over two views does not distinguish it from a shape half-outside
/// both (decision 0111).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShapeViewReport {
    pub view: String,
    /// Shapes clipped to this view's extent.
    pub clipped: u64,
    /// Shapes wholly outside this view's extent — **warned, never refused**: their membership
    /// there is empty and the operator decides (§4.3).
    pub outside: u64,
    pub rings_dropped: u64,
    pub degrees_looking: u64,
    pub interior_tiles: u64,
    pub boundary_cells: u64,
}

impl ShapeViewReport {
    /// Everything but the view's name — what two views are compared on to decide whether their
    /// frames made any difference.
    fn counts(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.clipped,
            self.outside,
            self.rings_dropped,
            self.degrees_looking,
            self.interior_tiles,
            self.boundary_cells,
        )
    }
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
            self.clipped,
            self.outside,
            self.rings_dropped,
            self.degrees_looking,
            self.children_escaping
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
        // **Per view, and only where the views disagree.** One view, or several agreeing, says
        // nothing a reader cannot read off the totals above; a difference between them is a
        // frame difference, which is what decision 0111 made possible and what an operator has to
        // be able to see.
        let differ = self
            .by_view
            .iter()
            .any(|v| v.counts() != self.by_view[0].counts());
        if self.by_view.len() > 1 && differ {
            for view in &self.by_view {
                eprintln!(
                    "    view '{}': clipped {}, wholly outside {}, rings dropped {}, \
                     degrees-looking {}; {} interior tile(s), {} boundary cell(s)",
                    view.view,
                    view.clipped,
                    view.outside,
                    view.rings_dropped,
                    view.degrees_looking,
                    view.interior_tiles,
                    view.boundary_cells
                );
            }
        }
        // **Warned, never a refusal** (§4.3): a shape wholly outside a view's extent holds no rows
        // there, is published, and the operator decides whether the extent or the geometry is
        // wrong. The number is per view, because that is the number that says which.
        for view in self.by_view.iter().filter(|v| v.outside > 0) {
            eprintln!(
                "    WARNING: {} of this layer's {} shape(s) lie wholly outside view '{}''s \
                 extent and hold no rows there; the other views are unaffected \
                 (`polygon-membership.md` §4.3)",
                view.outside, self.artifacts, view.view
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
///
/// **`space` is not one of them** — it is read beside these, by [`space_column`], because a table
/// may declare a space for geometry that has no column of the kind here: an authored shape content
/// is carried in a `contents` cell, and the space it is written in is its row's, exactly as a
/// membership shape's is (`polygon-membership.md` §6.1).
pub struct ShapeColumns<'a> {
    kind: ShapeKind,
    f64s: Vec<Option<&'a Float64Array>>,
    wkb: Option<Wkb<'a>>,
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
                    arrow::datatypes::DataType::Binary => {
                        Wkb::Binary(typed(path, array, "geometry")?)
                    }
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
        Ok(ShapeColumns { kind, f64s, wkb })
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
}

/// One artifact table's `space` column, where it carries one — the per-row override of the table's
/// `default_space` (`polygon-membership.md` §4.3).
///
/// Read apart from [`ShapeColumns`] because it governs **every** geometry the row declares, the
/// membership shape in the kind's own columns and the authored shape content in a `contents` cell
/// alike, and a layer carrying only the second has no [`ShapeColumns`] to hang it on.
pub fn space_column<'a>(
    path: &Path,
    batch: &'a RecordBatch,
    fields: &Fields,
) -> Result<Option<&'a StringArray>> {
    optional_utf8(path, batch, fields, "space")
}

/// The row's own `space` from that column, `None` where the row leaves it null.
pub fn space_at(column: Option<&StringArray>, row: usize) -> Option<&str> {
    column.filter(|c| !c.is_null(row)).map(|c| c.value(row))
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
    pub fn new(layer: &str, kind: ShapeKind, ctx: ShapeContext, default_space: ShapeSpace) -> Self {
        // **No warning for a layer over several views.** Spanning is opt-in and the caller
        // declared it, so the two-unprojected-views warning is removed (decision 0111); what is
        // warned is a shape outside a view's own extent, which is a fact about the geometry and
        // not about the declaration.
        ShapeReader {
            kind,
            report: ShapeLayerReport {
                layer: layer.to_string(),
                kind: kind.as_str().to_string(),
                views: ctx.views.iter().map(|v| v.view.clone()).collect(),
                by_view: ctx
                    .views
                    .iter()
                    .map(|v| ShapeViewReport {
                        view: v.view.clone(),
                        ..Default::default()
                    })
                    .collect(),
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
    /// is refused naming the row. A `wgs84` row is densified and put through the view's own
    /// projection before it is quantised (`polygon-membership.md` §4.3, R10).
    pub fn row(
        &mut self,
        key: &str,
        input: Option<ShapeInput>,
        space: Option<&str>,
    ) -> Result<Option<ArtifactShapes>> {
        let space = match space {
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
        let canonical = canonical_shapes(&shape, &self.ctx.views, space, self.ctx.max_vertices)
            .map_err(|e| {
                BuildError::Invalid(format!(
                    "layer '{}': artifact {key}: {e}",
                    self.report.layer
                ))
            })?;
        self.report.artifacts += 1;
        // **What the caller wrote is counted once; what a frame did is counted per frame.** Parts,
        // rings and the vertex counts describe the geometry of the declaration, so a layer drawn
        // on three views holds one polygon and not three; the clip, the out-of-extent count and
        // the decomposition's size are properties of a frame, and those are summed over the views
        // with the per-view rows keeping them apart (decision 0111). Where the frames differ the
        // output vertex count differs too, and the one reported is the first view's.
        if let Some((_, report, stats)) = canonical.reports.first() {
            let r = &mut self.report;
            r.parts += stats.parts;
            r.rings += stats.rings;
            r.vertices_in += report.vertices_in;
            r.vertices_out += report.vertices_out;
        }
        // **Every view's canonicalisation, not the first's** (decision 0111): where the frames
        // differ the results differ, and the layer's totals are the sum over its views while the
        // per-view rows keep them apart.
        for (view, report, stats) in &canonical.reports {
            self.note(view, report, stats);
        }
        self.report.canonical_bytes += canonical
            .by_view
            .iter()
            .map(|(v, b)| (v.len() + b.len()) as u64)
            .sum::<u64>();
        self.bounds
            .insert(key.to_string(), canonical.bounds.clone());
        Ok(ArtifactShapes::new(canonical.by_view))
    }

    fn note(
        &mut self,
        view: &str,
        report: &tessera_spatial::shape::CanonReport,
        stats: &ShapeStats,
    ) {
        if let Some(per_view) = self.report.by_view.iter_mut().find(|v| v.view == view) {
            per_view.clipped += u64::from(report.clipped);
            per_view.outside += u64::from(report.outside);
            per_view.rings_dropped += u64::from(report.rings_dropped);
            per_view.degrees_looking += u64::from(report.degrees_looking);
            per_view.interior_tiles += stats.interior_tiles;
            per_view.boundary_cells += stats.boundary_cells;
        }
        // Parts, rings and the vertex counts are **not** here: they are the caller's own
        // geometry, counted once per shape by [`ShapeReader::row`].
        let r = &mut self.report;
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
    pub fn finish(
        mut self,
        parents: impl IntoIterator<Item = (String, String)>,
    ) -> ShapeLayerReport {
        for (child, parent) in parents {
            let (Some(child), Some(parent)) = (self.bounds.get(&child), self.bounds.get(&parent))
            else {
                continue;
            };
            let escapes = child
                .iter()
                .zip(parent)
                .any(|((_, c), (_, p))| match (c, p) {
                    (Some(c), Some(p)) => {
                        c.min_x < p.min_x
                            || c.min_y < p.min_y
                            || c.max_x > p.max_x
                            || c.max_y > p.max_y
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
    let shape_layers = || config.layers.iter().any(|d| shape_declared(d).is_some());
    // **The views a build would materialise, not the `[[view]]` blocks alone.** A layer's `views`
    // may name a whole group, which is every view of it (`views.md` §2, §3.5) — resolved against
    // the plain blocks it would match nothing, and the layer would be sized over the rest of its
    // views with the group's silently dropped, span check included.
    let registry = match config.build_views() {
        Ok(registry) => registry,
        // Nothing here can be sized without the registry. Reported once rather than per layer,
        // and only where there is a shape layer to size at all.
        Err(why) => {
            if shape_layers() {
                out.push(Err(why.to_string()));
            }
            return out;
        }
    };
    for sources in &config.layer_sources {
        let Some(declaration) = config.layers.iter().find(|d| d.name == sources.name) else {
            continue;
        };
        let Some(kind) = shape_declared(declaration) else {
            continue;
        };
        // **A frame per view of the layer, not the first view's for all of them** (decision
        // 0111): `tessera check` sizes what the build will store, and where the frames differ so
        // do the decompositions. A view whose extent is `auto` has no frame until the points are
        // read, so the whole layer is reported unsized rather than half-sized.
        let drawn_on = Config::expand_layer_views(&registry, &declaration.views);
        let frames: std::result::Result<Vec<ViewFrame>, String> = drawn_on
            .iter()
            .filter_map(|name| registry.iter().find(|v| &v.id == name))
            .map(|view| match &view.extent {
                Extent::Fixed(bounds) => Ok(ViewFrame::new(&view.id, view.projection, *bounds)),
                // A stated longitude/latitude box is a frame without reading anything: the
                // projection and the snap are both functions of the declaration alone
                // (`projections.md` §4.2).
                Extent::LonLat(asked) => Ok(ViewFrame::new(
                    &view.id,
                    view.projection,
                    crate::config::snap_lon_lat(view.projection, asked)
                        .square
                        .bounds(),
                )),
                Extent::Auto { .. } | Extent::AutoLonLat => Err(format!(
                    "layer '{}': view '{}''s extent is `auto`, which is fitted to the points at \
                     the build; the shapes cannot be sized before then. Declare the extent to \
                     size them here",
                    declaration.name, view.id
                )),
            })
            .collect();
        let views = match frames {
            Ok(views) if views.is_empty() => continue,
            Ok(views) => views,
            Err(why) => {
                out.push(Err(why));
                continue;
            }
        };
        // The layer-level half of decision 0111's span rules, at the earliest place that can say
        // it: before a data file is opened. The row-level half needs the row's own `space`.
        if let Err(refusal) = check_shape_span(&views, ShapeSpace::Wgs84) {
            out.push(Err(format!("layer '{}': {refusal}", declaration.name)));
            continue;
        }
        let ctx = ShapeContext {
            views,
            max_vertices: DEFAULT_MAX_SHAPE_VERTICES,
        };
        let result = (|| -> Result<ShapeLayerReport> {
            let mut parents: Vec<(String, String)> = Vec::new();
            match &sources.artifacts {
                None => Ok(
                    ShapeReader::new(&declaration.name, kind, ctx, ShapeSpace::View)
                        .finish(parents),
                ),
                Some(ArtifactSource::Inline(rows)) => {
                    let mut reader =
                        ShapeReader::new(&declaration.name, kind, ctx, ShapeSpace::View);
                    for row in rows {
                        reader.row(&row.key, inline_shape(row, kind)?, row.space.as_deref())?;
                        for parent in &row.parent {
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
                    let mut reader = ShapeReader::new(&declaration.name, kind, ctx, *default_space);
                    for batch in crate::layers::batches(path)? {
                        let batch = batch?;
                        let keys = crate::layers::key_column(path, &batch, fields, "key")?;
                        let parent = crate::layers::parent_column(path, &batch, fields)?;
                        let columns = ShapeColumns::open(path, &batch, fields, kind)?;
                        let spaces = space_column(path, &batch, fields)?;
                        for row in 0..batch.num_rows() {
                            let key = crate::layers::key_at(&keys, row);
                            reader.row(
                                &key,
                                columns.at(path, row, &key)?,
                                space_at(spaces, row),
                            )?;
                            for parent in crate::layers::parents_at(path, parent.as_ref(), row)? {
                                parents.push((key.clone(), parent));
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
