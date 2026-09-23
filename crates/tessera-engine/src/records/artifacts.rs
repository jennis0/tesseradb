//! `POST /v1/artifacts`: a viewer reads, page by page, every artifact of one layer they are
//! served, with the properties they name. The response loop, the budgets, the page ends and the
//! cursor sealing are the items route's; this module is the walk.
//!
//! Rows are in `(level, ordinal)` order. Ordinals are claimed in publication order and never
//! reused or renumbered, so a position is a value that means the same artifact in every later
//! generation. An artifact's entity is not an order: a level's later runs sit at lower addresses.
//!
//! Every page resolves the layer again, composes the view's mask again and runs the one verdict,
//! [`crate::artifacts::ArtifactView::verdict`], for each artifact it walks, with the dependency
//! hook the viewport uses, so an attached artifact is served only while its target is. Content
//! decides servability as it does on browse. Nothing about the layer is gated or sorted whole per
//! page; the filter alone is evaluated over the whole view, once per response and again after a
//! publication renumbers the rows it is held in.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, Float64Builder, ListBuilder, StringBuilder, UInt32Builder,
    UInt64Builder,
};
use arrow::datatypes::{Field, Schema};
use arrow::record_batch::RecordBatch;
use croaring::Bitmap;
use tessera_types::layer::RegisteredLayer;
use tessera_types::{EntityId, TesseraId};

use super::columns::ViewFrame;
use super::cursor::{ArtifactsCursor, Binding, LayerBinding, Route};
use super::walk::{same_publication, Clock};
use super::{
    page_rows_of, refuse_shape, Counted, PageEndedBy, Paged, Pager, RecordsCounts,
    RecordsLimits, RecordsRefused, RecordsSink, RecordsTrailer, Response, ResponseEndedBy,
};
use crate::cancel::CancelToken;
use crate::derived::{ComputedProperty, DerivedContent, RowLocator};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::filter::FilterExpr;
use crate::gated_level::{check_level, GatedLevel, LayerRefusal};
use crate::histogram::MaskIdentity;
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::shapes::DrawnShape;
use crate::viewport::{filter_refusal, DependencyContext, OpenView};
use crate::Generation;


/// How many ordinals are walked between two readings of the clock.
const CHUNK: u32 = 1024;

/// One `POST /v1/artifacts` response, as the engine sees it.
#[derive(Debug, Clone)]
pub struct ArtifactsRequest<'a> {
    /// The view whose row space counts are taken in. One this session cannot reach is an unknown
    /// view.
    pub view: &'a str,
    /// A layer `/v1/meta` publishes to this viewer; an attached layer is allowed.
    pub layer: &'a str,
    /// Only artifacts at this level, on a levelled layer.
    pub level: Option<u32>,
    /// Only artifacts naming this one among their parents. One this viewer is not served names
    /// nothing, exactly as one with no children.
    pub parent: Option<TesseraId>,
    /// Only artifacts whose key, or first served text content, contains this, case-insensitively.
    pub q: Option<&'a str>,
    /// Only artifacts with a visible member matching this, and a `matched_count` column.
    pub filter: Option<FilterExpr>,
    /// Every served artifact under `filter`, those with no matching member included.
    pub keep_unmatched: bool,
    /// Put the served and matching counts in the head. Refused with a cursor.
    pub count: bool,
    /// Properties, by name, in the order their columns are wanted.
    pub fields: &'a [String],
    pub page_rows: Option<u32>,
    pub pages: Option<u32>,
    pub cursor: Option<&'a str>,
    pub idset: Option<u32>,
    pub limits: RecordsLimits,
    /// As [`super::ItemsRequest::cancel`].
    pub cancel: Option<CancelToken>,
}

/// A property a request can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Property {
    Key,
    Level,
    Parents,
    Target,
    MaskedCount,
    Content,
    Centroid,
    Box,
    Shape,
}

impl Property {
    fn parse(name: &str) -> Option<Property> {
        Some(match name {
            "key" => Property::Key,
            "level" => Property::Level,
            "parents" => Property::Parents,
            "target" => Property::Target,
            "masked_count" => Property::MaskedCount,
            "content" => Property::Content,
            "centroid" => Property::Centroid,
            "box" => Property::Box,
            "shape" => Property::Shape,
            _ => return None,
        })
    }
}

/// Why a page's walk stopped.
enum Ended {
    /// The page holds the page size.
    Filled,
    /// The next row would have taken it past the byte ceiling.
    Bytes,
    /// The clock stopped the response.
    Stopped(ResponseEndedBy),
    /// No artifact remains.
    End,
}

/// The filter's rows over the whole view, and the publication and mask they are held under.
struct HeldFilter {
    under: Arc<Generation>,
    mask: MaskIdentity,
    rows: Bitmap,
}

/// The artifacts route's pager.
struct ArtifactsPager<'r> {
    req: ArtifactsRequest<'r>,
    /// The layer's own entity when the read began: a layer registered since under the name is
    /// another layer, and the read ends.
    layer_entity: tessera_types::EntityId,
    properties: Vec<Property>,
    page_rows: u32,
    idset: u32,
    binding: Binding<'r>,
    /// The last `(level, ordinal)` returned or passed over.
    scan: Option<(u32, u32)>,
    /// The position the response started from: a stop is honoured only past it.
    origin: Option<(u32, u32)>,
    filter: Option<HeldFilter>,
    region: Option<RegionVerdict>,
}

/// One served artifact's row.
#[derive(Default)]
struct Row {
    tessera_id: u64,
    key: Option<String>,
    level: u32,
    parents: Vec<u64>,
    target: Option<u64>,
    masked_count: u64,
    content: Vec<String>,
    centroid: Option<(f64, f64)>,
    bbox: Option<[f64; 4]>,
    shape: Option<Vec<u8>>,
    matched: Option<u64>,
}

/// What an artifact is when the verdict serves it: its entity, its count and its content.
struct Served {
    entity: EntityId,
    masked_count: u64,
    content: Vec<String>,
}

/// One page's view of the layer: every level gated under the page's mask, and what a candidate
/// is tested against.
struct Scope<'a> {
    engine: &'a Engine,
    open: &'a OpenView<'a>,
    generation: &'a Generation,
    layer: &'a RegisteredLayer,
    levels: &'a [GatedLevel],
    views: &'a [crate::artifacts::ArtifactView<'a, crate::compose::EffectiveMask>],
    filter_rows: Option<&'a Bitmap>,
    /// Under `parent`: the parent's position where this viewer is served it, and `None` where no
    /// artifact is its child.
    parent: Option<Option<(u32, u32)>>,
    shard: u32,
}

impl Scope<'_> {
    /// The artifact at `(level, ordinal)` where this viewer is served it.
    fn served(&self, level: u32, ordinal: u32) -> Option<Served> {
        let gated = self.levels.get(level as usize)?;
        let entity = self.layer.runs[level as usize]
            .entity_of(u64::from(ordinal))
            .map(EntityId::new)?;
        let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
            self.views[level as usize].verdict(entity, ordinal)
        else {
            return None;
        };
        let content =
            gated.content(self.engine, self.generation, self.layer, ordinal, entity, rank)?;
        Some(Served {
            entity,
            masked_count,
            content,
        })
    }

    fn tessera_id(&self, entity: EntityId) -> Option<u64> {
        self.engine
            .identity_key
            .forward(self.shard, entity)
            .ok()
            .map(|id| id.raw())
    }

    /// Whether a served artifact passes `parent` and `q`: the narrowings that decide which served
    /// artifacts a read is of, before the filter.
    fn selected(&self, level: u32, ordinal: u32, served: &Served, q: Option<&str>) -> bool {
        if let Some(parent) = self.parent {
            let Some((p_level, p_ordinal)) = parent else {
                return false;
            };
            let names_it = self.levels[level as usize]
                .rows
                .parents(ordinal)
                .iter()
                .any(|p| p.level == p_level && p.ordinal == p_ordinal);
            if !names_it {
                return false;
            }
        }
        if let Some(q) = q {
            let needle = q.to_lowercase();
            let key = self.key(level, ordinal);
            let found = [key.as_deref(), served.content.first().map(String::as_str)]
                .into_iter()
                .flatten()
                .any(|text| text.to_lowercase().contains(&needle));
            if !found {
                return false;
            }
        }
        true
    }

    fn key(&self, level: u32, ordinal: u32) -> Option<String> {
        self.engine.write.live().with_artifacts(|store| {
            store
                .get(&self.layer.declaration.name, level, ordinal)?
                .key
                .clone()
        })
    }

    /// The artifact's visible members matching the filter, where there is one.
    fn matched(&self, level: u32, ordinal: u32) -> Option<u64> {
        let filter_rows = self.filter_rows?;
        Some(self.levels[level as usize].matched_count(ordinal, &self.open.mask, filter_rows))
    }
}

impl Engine {
    /// Serve one `POST /v1/artifacts` response into `sink` and return its trailer. Every refusal
    /// is decided before the head: the request's shape, the idset, the view, the layer and level,
    /// the cursor (before any position in it is used), the properties, then the filter.
    pub fn artifacts_stream(
        &self,
        session: &Session,
        req: ArtifactsRequest<'_>,
        sink: &mut dyn RecordsSink,
    ) -> Result<RecordsTrailer> {
        let started = std::time::Instant::now();
        refuse_shape(req.page_rows, req.pages, req.count, req.cursor)?;
        let (response, mut pager) = self.plan_artifacts(session, req)?;
        self.serve_pages(&response, &mut pager, started, sink)
    }

    fn plan_artifacts<'r>(
        &self,
        session: &'r Session,
        req: ArtifactsRequest<'r>,
    ) -> Result<(Response<'r>, ArtifactsPager<'r>)> {
        let refused = |why| Err(EngineError::RecordsRefused(why));
        if req.parent.is_some() && req.q.is_some() {
            return refused(RecordsRefused::ParentWithQ);
        }
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        let idset = manifest.identity.idset;
        if req.idset.is_some_and(|presented| presented != idset) {
            return Err(EngineError::StaleIdSet);
        }
        let unknown_view = || EngineError::UnknownView(req.view.to_string());
        if !session.visible_views().contains_view(req.view) {
            return Err(unknown_view());
        }
        let layer_refused = |refusal| {
            EngineError::RecordsRefused(match refusal {
                LayerRefusal::Unknown => RecordsRefused::UnknownLayer(req.layer.to_string()),
                LayerRefusal::OneLevel(_) => RecordsRefused::OneLevel(req.layer.to_string()),
                LayerRefusal::NoSuchLevel { held } => RecordsRefused::NoSuchLevel {
                    layer: req.layer.to_string(),
                    held,
                },
            })
        };
        let layer = self
            .readable_layer(session, &generation, req.layer, req.view)
            .map_err(layer_refused)?;
        check_level(&layer, req.level).map_err(layer_refused)?;
        let binding = Binding {
            route: Route::Artifacts,
            view: req.view,
            incarnation: manifest.incarnation_of(req.view).ok_or_else(unknown_view)?,
            auth_data_hash: session.auth_data_hash(),
            layer: Some(LayerBinding {
                name: req.layer,
                entity: layer.entity.raw(),
                level: req.level,
            }),
        };
        let resumed = match req.cursor {
            None => None,
            Some(token) => Some(ArtifactsCursor::decode(
                &self.cursor_key.open(&binding, token)?,
            )?),
        };
        if resumed.is_some_and(|cursor| cursor.idset != idset) {
            return Err(EngineError::StaleIdSet);
        }
        let mut properties: Vec<Property> = Vec::with_capacity(req.fields.len());
        for name in req.fields {
            let Some(property) = Property::parse(name) else {
                return refused(RecordsRefused::UnknownProperty(name.clone()));
            };
            if properties.contains(&property) {
                return refused(RecordsRefused::RepeatedField(name.clone()));
            }
            properties.push(property);
        }
        if let Some(expr) = &req.filter {
            generation
                .filter_columns
                .admit(expr, true, &|layer| self.reaches_layer(session, layer))
                .map_err(filter_refusal)?;
        }
        let page_rows = page_rows_of(req.page_rows, &req.limits);
        let scan = resumed.and_then(|cursor| cursor.scan);
        let response = Response {
            session,
            view: req.view,
            order: None,
            page_rows,
            pages: req.pages,
            count: req.count,
            limits: req.limits,
            cancel: req.cancel.clone(),
        };
        let pager = ArtifactsPager {
            req,
            layer_entity: layer.entity,
            properties,
            page_rows,
            idset,
            binding,
            scan,
            origin: scan,
            filter: None,
            region: None,
        };
        Ok((response, pager))
    }
}

impl ArtifactsPager<'_> {
    /// Run `f` over this page's scope. `None` where the layer is no longer served to this viewer,
    /// or is not the layer the read began on: nothing of it remains to read.
    fn with_scope<T>(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        f: impl FnOnce(&Self, &Scope<'_>) -> Result<T>,
    ) -> Result<Option<T>> {
        let served = &open.served;
        let Ok(layer) =
            engine.readable_layer(served.session, generation, self.req.layer, served.name)
        else {
            return Ok(None);
        };
        if layer.entity != self.layer_entity {
            return Ok(None);
        }
        if let Some(expr) = &self.req.filter {
            let stale = self.filter.as_ref().is_none_or(|held| {
                !same_publication(&held.under, generation)
                    || held.mask != served.mask_identity
            });
            if stale {
                let (rows, region) =
                    engine.whole_view_filter_rows(served, &open.mask, expr, &self.req.cancel)?;
                self.region = RegionVerdict::coarsest(self.region, region);
                self.filter = Some(HeldFilter {
                    under: Arc::clone(generation),
                    mask: served.mask_identity,
                    rows,
                });
            }
        }
        let filter_rows = self.filter.as_ref().map(|held| &held.rows);
        let geometry = self
            .properties
            .iter()
            .any(|p| matches!(p, Property::Centroid | Property::Box));
        let levels: Vec<GatedLevel> = (0..layer.runs.len() as u32)
            .map(|level| engine.gated_level(served, &open.mask, &layer, level, filter_rows, geometry))
            .collect();
        let reachable = engine.reachable_layers(served.session);
        let ctx = DependencyContext::new(served, &open.mask, &reachable);
        let dependency_served = engine.dependency_gate(&ctx);
        let views: Vec<_> = levels
            .iter()
            .map(|level| level.view(engine, served, &open.mask, &layer, &dependency_served))
            .collect();
        let mut scope = Scope {
            engine,
            open,
            generation,
            layer: &layer,
            levels: &levels,
            views: &views,
            filter_rows,
            parent: None,
            shard: generation.bundle.manifest.identity.shard_id,
        };
        if let Some(parent) = self.req.parent {
            scope.parent = Some(parent_position(&scope, parent));
        }
        f(self, &scope).map(Some)
    }

    /// The levels the read walks, in order.
    fn levels(&self, layer: &RegisteredLayer) -> std::ops::Range<u32> {
        match self.req.level {
            Some(level) => level..level + 1,
            None => 0..layer.runs.len() as u32,
        }
    }

    /// Build the row for a served artifact that the request selects, with the properties named.
    fn row(&self, scope: &Scope<'_>, level: u32, ordinal: u32, served: Served) -> Result<Row> {
        let gated = &scope.levels[level as usize];
        let mut row = Row {
            tessera_id: scope.tessera_id(served.entity).unwrap_or_default(),
            level,
            masked_count: served.masked_count,
            matched: scope.matched(level, ordinal),
            ..Row::default()
        };
        let frame = || ViewFrame::of(scope.generation, scope.open.served.name);
        for property in &self.properties {
            match property {
                Property::Key => row.key = scope.key(level, ordinal),
                Property::Level | Property::MaskedCount => {}
                Property::Parents => {
                    let mut ids: Vec<u64> = gated
                        .rows
                        .parents(ordinal)
                        .iter()
                        .filter_map(|p| {
                            let parent = scope.served(p.level, p.ordinal)?;
                            scope.tessera_id(parent.entity)
                        })
                        .collect();
                    ids.sort_unstable();
                    ids.dedup();
                    row.parents = ids;
                }
                Property::Target => {
                    // The verdict served this artifact only because its target is served.
                    row.target = gated
                        .rows
                        .attachment(ordinal)
                        .and_then(|a| scope.tessera_id(a.entity));
                }
                Property::Content => row.content = served.content.clone(),
                Property::Centroid | Property::Box => {
                    if row.centroid.is_none() && row.bbox.is_none() {
                        let derived = visible_geometry(scope, gated, ordinal);
                        let frame = frame()?;
                        row.centroid = derived.centroid.map(|[x, y]| frame.point(x, y));
                        row.bbox = derived.bbox.map(|[x0, y0, x1, y1]| {
                            let (ax, ay) = frame.point(f64::from(x0), f64::from(y0));
                            let (bx, by) = frame.point(f64::from(x1), f64::from(y1));
                            [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
                        });
                    }
                }
                Property::Shape => {
                    row.shape = drawn_shape(scope, gated, ordinal, &served.content)
                        .map(|parts| -> Result<Vec<u8>> {
                            let frame = frame()?;
                            let parts: tessera_spatial::shape::RingsF64 = parts
                                .into_iter()
                                .map(|rings| {
                                    rings
                                        .into_iter()
                                        .map(|ring| {
                                            ring.into_iter()
                                                .map(|[x, y]| {
                                                    frame.point(f64::from(x), f64::from(y))
                                                })
                                                .collect()
                                        })
                                        .collect()
                                })
                                .collect();
                            Ok(tessera_spatial::shape::write_wkb(&parts))
                        })
                        .transpose()?;
                }
            }
        }
        Ok(row)
    }

    /// The Arrow bytes a row adds to a page: its values, offsets and validity.
    fn row_bytes(&self, row: &Row) -> usize {
        let mut bytes = 8 + usize::from(self.req.filter.is_some()) * 8;
        for property in &self.properties {
            bytes += match property {
                Property::Key => 5 + row.key.as_ref().map_or(0, String::len),
                Property::Level => 4,
                Property::Parents => 5 + 8 * row.parents.len(),
                Property::Target | Property::MaskedCount => 9,
                Property::Content => 4 + row.content.iter().map(|c| 4 + c.len()).sum::<usize>(),
                Property::Centroid => 17,
                Property::Box => 33,
                Property::Shape => 5 + row.shape.as_ref().map_or(0, Vec::len),
            };
        }
        bytes
    }

    /// The page's rows as one batch: `tessera_id`, the properties in the order named, then
    /// `matched_count` under a filter.
    fn batch(&self, rows: &[Row]) -> Result<RecordBatch> {
        let mut fields: Vec<Field> = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        let mut push = |name: &str, array: ArrayRef, nullable: bool| {
            fields.push(Field::new(name, array.data_type().clone(), nullable));
            arrays.push(array);
        };
        push(
            "tessera_id",
            Arc::new(rows.iter().map(|r| r.tessera_id).collect::<arrow::array::UInt64Array>()),
            false,
        );
        let f64s = |at: fn(&Row) -> Option<f64>| -> ArrayRef {
            let mut b = Float64Builder::with_capacity(rows.len());
            for row in rows {
                b.append_option(at(row));
            }
            Arc::new(b.finish())
        };
        for property in &self.properties {
            match property {
                Property::Key => {
                    let mut b = StringBuilder::new();
                    for row in rows {
                        b.append_option(row.key.as_deref());
                    }
                    push("key", Arc::new(b.finish()), true);
                }
                Property::Level => {
                    let mut b = UInt32Builder::with_capacity(rows.len());
                    for row in rows {
                        b.append_value(row.level);
                    }
                    push("level", Arc::new(b.finish()), false);
                }
                Property::Parents => {
                    let mut b = ListBuilder::new(UInt64Builder::new());
                    for row in rows {
                        b.values().append_slice(&row.parents);
                        b.append(true);
                    }
                    push("parents", Arc::new(b.finish()), false);
                }
                Property::Target => {
                    let mut b = UInt64Builder::with_capacity(rows.len());
                    for row in rows {
                        b.append_option(row.target);
                    }
                    push("target", Arc::new(b.finish()), true);
                }
                Property::MaskedCount => {
                    let mut b = UInt64Builder::with_capacity(rows.len());
                    for row in rows {
                        b.append_value(row.masked_count);
                    }
                    push("masked_count", Arc::new(b.finish()), false);
                }
                Property::Content => {
                    let mut b = ListBuilder::new(StringBuilder::new());
                    for row in rows {
                        for value in &row.content {
                            b.values().append_value(value);
                        }
                        b.append(true);
                    }
                    push("content", Arc::new(b.finish()), false);
                }
                Property::Centroid => {
                    push("centroid_x", f64s(|r| r.centroid.map(|c| c.0)), true);
                    push("centroid_y", f64s(|r| r.centroid.map(|c| c.1)), true);
                }
                Property::Box => {
                    push("box_x_min", f64s(|r| r.bbox.map(|b| b[0])), true);
                    push("box_y_min", f64s(|r| r.bbox.map(|b| b[1])), true);
                    push("box_x_max", f64s(|r| r.bbox.map(|b| b[2])), true);
                    push("box_y_max", f64s(|r| r.bbox.map(|b| b[3])), true);
                }
                Property::Shape => {
                    let mut b = BinaryBuilder::new();
                    for row in rows {
                        b.append_option(row.shape.as_deref());
                    }
                    push("shape", Arc::new(b.finish()), true);
                }
            }
        }
        if self.req.filter.is_some() {
            let mut b = UInt64Builder::with_capacity(rows.len());
            for row in rows {
                b.append_value(row.matched.unwrap_or(0));
            }
            push("matched_count", Arc::new(b.finish()), false);
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
            .map_err(|e| EngineError::Malformed(format!("an artifacts page did not assemble: {e}")))
    }
}

/// Where the requested parent sits, where this viewer is served it; `None` otherwise, which
/// selects nothing, as a parent with no children does.
fn parent_position(scope: &Scope<'_>, parent: TesseraId) -> Option<(u32, u32)> {
    let (shard, entity) = scope.engine.identity_key.invert(parent);
    if shard != scope.shard {
        return None;
    }
    let (name, level, ordinal) = scope.engine.write.live().locate_artifact(entity)?;
    if name != scope.layer.declaration.name {
        return None;
    }
    let served = scope.served(level, ordinal)?;
    (served.entity == entity).then_some((level, ordinal))
}

/// The artifact's centroid and box over the members this viewer can see, in grid units: read off
/// the level's accumulation where it has one, and computed from the visible rows otherwise.
fn visible_geometry(scope: &Scope<'_>, gated: &GatedLevel, ordinal: u32) -> DerivedContent {
    let wanted = [ComputedProperty::Centroid, ComputedProperty::Box];
    if let Some(geometry) = gated.counts.as_ref().and_then(|c| c.geometry()) {
        return crate::derived::accumulated(&wanted, geometry, ordinal);
    }
    let locator = RowLocator::new(scope.open.served.segments.clone());
    let visible = gated.rows.visible_rows(ordinal, &scope.open.mask);
    crate::derived::compute(&wanted, &visible, &locator)
}

/// The layer's drawn geometry for one artifact, in grid units: a derived hull over the members
/// this viewer can see, or the predicate or authored shape whole, as the drill-down serves it.
fn drawn_shape(
    scope: &Scope<'_>,
    gated: &GatedLevel,
    ordinal: u32,
    content: &[String],
) -> Option<Vec<Vec<Vec<[u32; 2]>>>> {
    let declaration = &scope.layer.declaration;
    match declaration.drawn_shape()? {
        DrawnShape::Derived => {
            let locator = RowLocator::new(scope.open.served.segments.clone());
            let visible = gated.rows.visible_rows(ordinal, &scope.open.mask);
            crate::derived::compute(&[ComputedProperty::Hull], &visible, &locator).shape
        }
        DrawnShape::Predicate | DrawnShape::Authored => {
            let mut content = content.to_vec();
            let mut derived = DerivedContent::default();
            scope.engine.drawn_shape(
                declaration,
                scope.open.served.name,
                &declaration.name,
                gated.level,
                ordinal,
                &mut content,
                &mut derived,
                None,
            );
            derived.shape
        }
    }
}

impl Pager for ArtifactsPager<'_> {
    /// The artifacts the request selects that this viewer is served, and of them the ones with a
    /// visible member matching the filter, over the whole layer under the first page's mask.
    fn count(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
    ) -> Result<Counted> {
        let counted = self.with_scope(engine, open, generation, |pager, scope| {
            let (mut served_count, mut matched) = (0u64, 0u64);
            for level in pager.levels(scope.layer) {
                for ordinal in 0..scope.levels[level as usize].rows.len() as u32 {
                    let Some(served) = scope.served(level, ordinal) else {
                        continue;
                    };
                    if !scope.selected(level, ordinal, &served, pager.req.q) {
                        continue;
                    }
                    served_count += 1;
                    if scope.matched(level, ordinal).is_none_or(|n| n > 0) {
                        matched += 1;
                    }
                }
            }
            Ok((served_count, matched))
        })?;
        let (served, matched) = counted.unwrap_or((0, 0));
        Ok((RecordsCounts { served, matched }, self.region))
    }

    /// The artifacts past the position in `(level, ordinal)` order that the verdict serves and
    /// the request selects, up to the page size and the byte ceiling. The clock is read between
    /// chunks of the walk and honoured once the position has moved.
    fn page(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        clock: &mut Clock,
    ) -> Result<Paged> {
        let max_bytes = self.req.limits.max_page_bytes;
        let walked = self.with_scope(engine, open, generation, |pager, scope| {
            let mut rows: Vec<Row> = Vec::new();
            let mut bytes = 0usize;
            let mut scan = pager.scan;
            let mut since_clock = 0u32;
            for level in pager.levels(scope.layer) {
                let start = match scan {
                    Some((at, _)) if at > level => continue,
                    Some((at, ordinal)) if at == level => ordinal.saturating_add(1),
                    _ => 0,
                };
                let len = scope.levels[level as usize].rows.len() as u32;
                for ordinal in start..len {
                    if since_clock == 0 {
                        if let Some(reason) = clock.stop() {
                            if !rows.is_empty() || scan > pager.origin {
                                return Ok((rows, bytes, scan, Ended::Stopped(reason)));
                            }
                        }
                    }
                    since_clock = (since_clock + 1) % CHUNK;
                    let row = match scope.served(level, ordinal) {
                        Some(served) if scope.selected(level, ordinal, &served, pager.req.q) => {
                            let unmatched = scope.matched(level, ordinal) == Some(0);
                            if unmatched && !pager.req.keep_unmatched {
                                None
                            } else {
                                Some(pager.row(scope, level, ordinal, served)?)
                            }
                        }
                        _ => None,
                    };
                    if let Some(row) = row {
                        let row_bytes = pager.row_bytes(&row);
                        if !rows.is_empty() && bytes + row_bytes > max_bytes {
                            return Ok((rows, bytes, scan, Ended::Bytes));
                        }
                        bytes += row_bytes;
                        rows.push(row);
                    }
                    scan = Some((level, ordinal));
                    if rows.len() == pager.page_rows as usize {
                        return Ok((rows, bytes, scan, Ended::Filled));
                    }
                }
            }
            Ok((rows, bytes, scan, Ended::End))
        })?;
        let Some((rows, bytes, scan, ended)) = walked else {
            return Ok(Paged::End);
        };
        self.scan = scan;
        if rows.is_empty() {
            return Ok(match ended {
                Ended::Stopped(reason) => Paged::Stopped(reason),
                _ => Paged::End,
            });
        }
        let (ended_by, then) = match ended {
            Ended::Filled => (PageEndedBy::Rows, None),
            Ended::Bytes => (PageEndedBy::Bytes, None),
            Ended::Stopped(reason) => (PageEndedBy::Time, Some(reason)),
            Ended::End => (PageEndedBy::End, None),
        };
        Ok(Paged::Rows {
            batch: self.batch(&rows)?,
            bytes,
            ended_by,
            then,
        })
    }

    fn cursor(&self, engine: &Engine) -> String {
        let cursor = ArtifactsCursor {
            idset: self.idset,
            scan: self.scan,
        };
        engine.cursor_key.seal(&self.binding, &cursor.encode())
    }

    fn region(&self) -> Option<RegionVerdict> {
        self.region
    }
}
