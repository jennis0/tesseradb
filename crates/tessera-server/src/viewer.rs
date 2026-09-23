//! The viewer plane: `/v1/meta`, `/v1/categories`, `/v1/viewport`, `/v1/items`,
//! `/v1/items/{tessera_id}` and `/v1/artifacts`, plus `/healthz` and `/readyz`. Bearer auth is a session token minted by the
//! session plane's `/session/authorise`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use tessera_types::{GenerationStamp, TesseraId};
use tessera_wire::{
    artifacts_frame, artifacts_identity_frame, points_frame, points_highlight_frame,
    sub_cells_frame, tiles_frame,
    trailer_frame, ArtifactRow, ScalarColumn,
};

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{
    CancelToken, ComputedSelection, LayerSelection, LevelSelection, SinkResult,
    ViewportHead, ViewportSink,
};

use crate::error::{map_engine_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::{ApiJson, AppState, GatePermits, ViewerSession};
use crate::stream::{CancelGuard, Producer};

pub fn router(state: Arc<AppState>) -> Router {
    // With neither CORS list set there is no CORS layer at all.
    let cors = crate::cors::viewer_layer(&state);
    let router = Router::new()
        .route("/v1/meta", get(meta))
        .route("/v1/categories/{column}", get(categories))
        .route("/v1/categories/{column}/suggest", get(suggest))
        .route("/v1/viewport", post(viewport))
        .route("/v1/items", post(crate::records::items))
        .route("/v1/items/{tessera_id}", post(item))
        .route("/v1/artifacts", post(crate::records::artifacts))
        .route("/v1/artifacts/{tessera_id}", post(artifact))
        .route("/v1/artifacts/browse", post(browse))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // Trims the allocator after responses; mounted on every plane, so a node that never
        // ingests still returns what serving grew.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::memory::trim_after_response,
        ))
        .with_state(state);
    match cors {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

/// A generation stamp on the wire, in the viewport request body and the `x-tessera-pin` response
/// header (the same JSON in both). The server always answers from live geometry; the stamp only
/// sets the response's `stale` flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PinDto {
    prefix: String,
    segments_version: u64,
}

impl From<PinDto> for GenerationStamp {
    fn from(p: PinDto) -> Self {
        GenerationStamp {
            prefix: p.prefix,
            segments_version: p.segments_version,
        }
    }
}

impl From<&GenerationStamp> for PinDto {
    fn from(p: &GenerationStamp) -> Self {
        PinDto {
            prefix: p.prefix.clone(),
            segments_version: p.segments_version,
        }
    }
}

/// One roster metadata value on the wire: `{"type": …, "value": …}`. Written out rather than
/// derived so the published shape lives here; a `timestamp_us` is microseconds since the epoch.
fn metadata_value(value: &tessera_engine::ViewMetadataValue) -> serde_json::Value {
    use tessera_engine::ViewMetadataValue as V;
    let (tag, value) = match value {
        V::Bool(v) => ("bool", serde_json::json!(v)),
        V::Int(v) => ("int", serde_json::json!(v)),
        V::Float(v) => ("float", serde_json::json!(v)),
        V::Text(v) => ("text", serde_json::json!(v)),
        V::TimestampUs(v) => ("timestamp_us", serde_json::json!(v)),
    };
    serde_json::json!({"type": tag, "value": value})
}

/// A column's `category` block on `/v1/meta`: the vocabulary it draws from, that vocabulary's kind
/// and its visibility. `None` for a column with no vocabulary, or one naming a vocabulary the
/// snapshot does not hold.
fn category_block(
    meta: &tessera_engine::EngineMeta,
    vocabulary: Option<&str>,
) -> Option<serde_json::Value> {
    let name = vocabulary?;
    let vocabulary = meta.vocabularies.get(name)?;
    Some(serde_json::json!({
        "vocabulary": name,
        "kind": match vocabulary.kind() {
            tessera_engine::VocabularyKind::Declared => "declared",
            tessera_engine::VocabularyKind::Discovered => "discovered",
        },
        "visibility": vocabulary.visibility().as_str(),
    }))
}

/// `GET /v1/meta`. It needs a session like every other route on this plane: it discloses the
/// bundle's extents, views and declared-scalar schema.
async fn meta(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let meta = state.engine.meta();
    let selection = state.engine.config();
    // The layers, views, groups and scoped families below are filtered per principal, so this
    // response must not be shared between principals. A layer the caller may not know about is
    // absent exactly as an unregistered one is.
    let layers = state.engine.visible_layers(&session);
    // Resolved at authorise and fixed for the session, so this roster agrees with what every
    // viewer route answers.
    let visible = session.visible_views();
    Ok(Json(serde_json::json!({
        "api_version": meta.api_version,
        "bundle_format": meta.bundle_format,
        // The idset `POST /v1/items` checks against. The identity key itself never appears in a
        // response, log line or metric label.
        "idset": meta.idset,
        // The views this principal may reach, in creation order and carrying no position, so a
        // shorter list reveals nothing about what was withheld. A null `tile_scheme` means draw
        // no basemap.
        "views": meta.views.iter().filter(|v| visible.contains_view(&v.id)).map(|v| serde_json::json!({
            "id": v.id,
            "display_name": v.display_name,
            // The frame this view's positions are quantised against and its tile prefixes decode
            // with; per view, since an embedding and a map cannot usefully share one.
            "quantisation": {
                "x_min": v.quantisation.x_min,
                "x_max": v.quantisation.x_max,
                "y_min": v.quantisation.y_min,
                "y_max": v.quantisation.y_max,
            },
            "projection": v.projection.name(),
            // The aspect ratio to draw the world at: 1 for `web_mercator`, `2cos φ₁` for an
            // equirectangular alias, null for `none`.
            "world_aspect": v.projection.world_aspect(),
            "tile_scheme": v.tile.map(|t| t.scheme),
            "tile": v.tile.map(|t| serde_json::json!({"z": t.z, "x": t.x, "y": t.y})),
            // The roster record, null on a view outside any group. `metadata` has one typed
            // `{type, value}` entry per name the group declared.
            "group": v.roster.as_ref().map(|r| &r.group),
            "key": v.roster.as_ref().map(|r| &r.key),
            "metadata": v.roster.as_ref().map(|r| r.metadata.iter().map(|(name, value)| {
                (name.clone(), metadata_value(value))
            }).collect::<serde_json::Map<_, _>>()),
        })).collect::<Vec<_>>(),
        // Groups in manifest order, each listing its reachable views in creation order as
        // `group:key` ids, so a client can step between views without interpreting keys. A group
        // the principal cannot reach is absent with all its views.
        "groups": meta.groups.iter().filter(|g| visible.contains_group(&g.name)).map(|g| serde_json::json!({
            "name": g.name,
            "title": g.title,
            "members_of": g.members_of,
            "views": g.views.iter().filter(|id| visible.contains_view(id)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        // The whole column schema. The `category` block is what tells a `u16` category from a
        // `u16` integer, since the points batch carries only codes. Values are served by the
        // paged, per-principal `/v1/categories`, which keeps this document small.
        "declared_scalars": meta.declared_scalars.iter().enumerate().map(|(index, s)| {
            serde_json::json!({
                "name": s.name,
                "arrow_type": s.arrow_type.arrow_type_name(),
                "category": category_block(&meta, s.vocabulary.as_deref()),
                // The `<name>/<version>` of the analyser that produced a `text` column's terms,
                // null for other types. With it a client can reproduce the segmentation (`tessera
                // tokenise`) and tell "no match" from "segmented differently".
                "analyser": s.analyser,
                // `render`: a slot in every row of the hot column; `index`: an entity-space search
                // structure; neither: stored and returned on drill-down, but not filterable.
                "render": s.render,
                "index": s.index,
                // Where `POST /v1/items` reads the value from; a field whose only home is
                // `record` reads fastest in stored order.
                "homes": meta.homes[index].names(),
            })
        }).collect::<Vec<_>>(),
        // Group-scoped column families, in `declared_scalars`' shape plus `scope`: each family is
        // one column per view of its group, and `render` promises the column under those views
        // only. A family whose group this principal cannot reach is absent, as in the operand list.
        "scoped_scalars": meta.scoped_scalars.iter().filter(|f| visible.contains_group(&f.group)).map(|f| {
            serde_json::json!({
                "name": f.name,
                "arrow_type": f.arrow_type.arrow_type_name(),
                "scope": {"group": f.group},
                "category": category_block(&meta, f.vocabulary.as_deref()),
                "analyser": f.analyser,
                "render": f.render,
                "index": f.index,
                // The views whose rows carry the column and this principal may reach: the group's
                // views and those of groups declaring it in `members`. A view created while running
                // joins only once a flush carrying its values publishes.
                "views": meta
                    .scoped_family_views(f)
                    .into_iter()
                    .filter(|id| visible.contains_view(id))
                    .collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        // Filterable columns (`filter::is_filterable`, as the viewport parse uses) with their
        // family's operators; `none_of` is refused over a `text` column, which this does not say.
        // A scoped entry must be pinned outside its group's views; unreachable groups are omitted.
        "filter_operands": meta.declared_scalars.iter().filter(|d| tessera_engine::filter::is_filterable(d)).map(|d| {
            let family = tessera_engine::filter::Family::of(d);
            serde_json::json!({
                "column": d.name,
                "family": family.as_str(),
                "operands": family.operands(),
            })
        }).chain(
            meta.scoped_scalars.iter().filter(|f| tessera_engine::filter::scoped_is_filterable(f) && visible.contains_group(&f.group)).map(|f| {
                let family = tessera_engine::filter::Family::of_scoped(f);
                serde_json::json!({
                    "column": f.name,
                    "family": family.as_str(),
                    "operands": family.operands(),
                    "scope": {"group": f.group},
                    // A scoped family's vocabulary and analyser are on its `scoped_scalars` entry.
                })
            })
        ).collect::<Vec<_>>(),
        // Deployment constants, the same for every principal, so they disclose nothing. A client
        // needs them to read mark count as density and to tell its own request's bound from the
        // deployment's ceiling (`min(k, max_k, k_max_marks)`).
        "selection": {
            "k_min": selection.k_min,
            "k_max_marks": selection.k_max_marks,
            "max_k": state.limits.max_k,
            "theta_target_marks": selection.theta_target_marks,
            "max_underlay_offset": selection.max_underlay_offset,
            "max_tiles_per_request": selection.max_tiles_per_request,
            "max_category_values": state.limits.max_category_values,
            // `/v1/categories/{column}/suggest`'s page ceiling (also `limit`'s default) and walk
            // budget; a page with `more: true` that is not full hit the walk budget.
            "max_suggestions": state.limits.max_suggestions,
            "max_suggestion_walk": state.limits.max_suggestion_walk,
            // At or under this many visible entities a suggestion is answered from the session's
            // own values rather than by probing. Comparing it with one's own `visible` count
            // discloses only one's own mask.
            "max_suggest_set_entities": state.limits.max_suggest_set_entities,
            // The vertex cap for publishing a shape.
            "max_shape_vertices": state.limits.max_shape_vertices,
            // The `region` leaf's bounds: over the vertex cap is a 422; over the cell cap the
            // answer is a cover, reported in `x-tessera-region`, not a refusal.
            "max_region_vertices": state.limits.max_region_vertices,
            "max_region_cells": state.limits.max_region_cells,
            // `POST /v1/artifacts/browse`'s page ceiling and default.
            "max_browse_rows": state.limits.max_browse_rows,
            // `POST /v1/items`' page ceilings: rows, and Arrow bytes before compression.
            "max_page_rows": state.limits.max_page_rows,
            "max_page_bytes": state.limits.max_page_bytes,
        },
        // The layers this principal may know exist, with what each declared. Never a layer's
        // artifact count, which counts objects the principal may not see, and never its gate
        // label, which would name a term on a document meant to hide unreachable layers.
        "layers": layers.iter().map(|layer| {
            let d = &layer.declaration;
            serde_json::json!({
                "name": d.name,
                "title": d.title,
                // Only the views this principal may reach.
                "views": d.views.iter().filter(|id| visible.contains_view(id)).collect::<Vec<_>>(),
                "membership": d.membership,
                "hierarchy": {
                    "kind": d.hierarchy.kind,
                    // The default cut depth; a viewport request may ask for a deeper one.
                    "prune_children": d.hierarchy.prune_children,
                },
                // Empty for a treed layer: its lineage is in its edges.
                "levels": d.levels.iter().map(|l| serde_json::json!({
                    "level": l.level,
                    "title": l.title,
                    "zoom": l.zoom.map(|(lo, hi)| serde_json::json!([lo, hi])),
                })).collect::<Vec<_>>(),
                "computed_content": d.content.computed,
                // The drawn geometry's kind: `derived` (a hull over visible members, per
                // principal), `predicate` or `authored` (the same for every principal), or null.
                // Only a shape that is not derived may be held across principals.
                "shape": d.drawn_shape().map(|k| k.name()),
                // Supplied content types only; each entry's declared `name` is not published yet.
                "supplied_content": d.content.supplied.iter()
                    .map(|s| s.ty.clone()).collect::<Vec<_>>(),
                "depends_on": d.depends_on,
                // The version a client echoes to notice a gate edit, in the same shape as every
                // other version coordinate it holds.
                "version": layer.version,
            })
        }).collect::<Vec<_>>(),
    })))
}

/// `GET /v1/categories/{column}`'s query string.
#[derive(Debug, Deserialize)]
struct CategoriesQuery {
    /// The request's view, which addresses a group-scoped category's per-view value set; unused
    /// for an entity-scoped column. A view the session cannot reach is a 404.
    #[serde(default)]
    view: Option<String>,
    /// Comma-separated codes to resolve. Present means bulk lookup; absent means enumerate.
    #[serde(default)]
    codes: Option<String>,
    /// Resume enumeration after this value key: the cursor.
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /v1/categories/{column}`: what a column's codes stand for, for `?codes=` or as a paged
/// listing. Both forms apply `visibility` in `Engine::categories`. No such column, a non-category
/// column and a missing vocabulary are one indistinguishable 404.
async fn categories(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    AxumPath(column): AxumPath<String>,
    AxumQuery(query): AxumQuery<CategoriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let meta = state.engine.meta();
    let visible = session.visible_views();
    let resolved =
        resolve_category_column(&meta, &column, query.view.as_deref(), visible)?;

    // Clamped rather than refused, since the cursor carries the rest; `0` is refused because a
    // zero-length page never advances.
    let limit = match query.limit {
        Some(0) => {
            return Err(ApiError::Contract(
                "limit must be at least 1".to_string(),
            ))
        }
        Some(n) => n.min(state.limits.max_category_values),
        None => state.limits.max_category_values,
    };

    // Parsed before the engine call so a malformed code list is a 422 about the request rather
    // than an empty 200 that reads as "you may see none of these".
    let codes: Option<Vec<u32>> = match &query.codes {
        Some(raw) if raw.is_empty() => Some(Vec::new()),
        Some(raw) => Some(
            raw.split(',')
                .map(|c| c.trim().parse::<u32>())
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| {
                    ApiError::Contract(format!(
                        "`codes` must be a comma-separated list of u32: {e}"
                    ))
                })?,
        ),
        None => None,
    };

    // Off the compute gate, but a `derived` column probes a posting per value, so this runs on a
    // blocking thread.
    let after = query.after;
    let page = state
        .blocking(move |state| {
            let query = match &codes {
                Some(codes) => tessera_engine::CategoryQuery::Codes(codes),
                None => tessera_engine::CategoryQuery::Page {
                    after: after.as_deref(),
                    limit,
                },
            };
            state
                .engine
                .categories(&session, &resolved, query)
                .map_err(map_engine_error)
        })
        .await?
        .ok_or_else(|| ApiError::Unknown("unknown category column".to_string()))?;

    Ok(Json(serde_json::json!({
        // The caller's own spelling, not the engine-internal resolved name of a scoped family.
        "column": column,
        "values": page.values.iter().map(|v| serde_json::json!({
            "code": v.code,
            "key": v.key,
            // Null when no author wrote a title, as for every discovered value; the key is the
            // display fallback.
            "title": v.title,
        })).collect::<Vec<_>>(),
        "next": page.next,
    })))
}

/// Resolves the column for `/v1/categories/{column}` and its `suggest` route, so the two cannot
/// disagree on what a spelling names. A scoped family is addressed by `?view=` or a
/// `{column}@{key}` pin; a view the session cannot reach is the unknown-view 404.
fn resolve_category_column(
    meta: &tessera_engine::EngineMeta,
    column: &str,
    requested_view: Option<&str>,
    visible: &tessera_engine::gate::VisibleViews,
) -> Result<String, ApiError> {
    // `view` is resolved before the column, so an unknown or unreachable view is a 404 even for
    // an entity-scoped column.
    let view = match requested_view {
        None => "",
        Some(requested) => match meta.resolve_visible_view(requested, visible) {
            Some(view) => view.id.as_str(),
            None => return Err(ApiError::Unknown(format!("unknown view '{requested}'"))),
        },
    };
    match meta.resolve_category_column(column, view, visible) {
        // A non-category column gets the same 404 as no column at all.
        tessera_engine::LeafColumn::Resolved {
            column: resolved,
            family: tessera_engine::filter::Family::Category,
        } => Ok(resolved),
        tessera_engine::LeafColumn::Unpinned { group } => Err(ApiError::Contract(format!(
            "'{column}' is scoped to view group '{group}' and this request names no view of \
             it; pass `view=` a view of that group, or pin the one it means as '{column}@<key>'"
        ))),
        tessera_engine::LeafColumn::UnknownPin { group, pin } => Err(ApiError::Unknown(format!(
            "unknown view '{pin}' of group '{group}'"
        ))),
        tessera_engine::LeafColumn::PinOnUnscoped { column } => Err(ApiError::Contract(format!(
            "'{column}' is not scoped to a view group; leave out the pin"
        ))),
        _ => Err(ApiError::Unknown("unknown category column".to_string())),
    }
}

/// `GET /v1/categories/{column}/suggest`'s query string.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestQuery {
    /// The text typed, echoed back as received, not folded. At most 256 bytes; empty matches
    /// every value.
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    counts: Option<bool>,
    /// As `/v1/categories`' `view`, resolved the same way.
    #[serde(default)]
    view: Option<String>,
}

/// `GET /v1/categories/{column}/suggest`: the values whose folded key or title, or a word start in
/// either, has `q` as a prefix. Who may see a value is decided as for `/v1/categories`. Unknown
/// parameters and `limit=0` are 422s.
async fn suggest(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    AxumPath(column): AxumPath<String>,
    query: Result<AxumQuery<SuggestQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // An unknown parameter fails extraction; answer with this route's 422 and a fixed detail
    // rather than axum's plain-text rejection or the caller-supplied serde error.
    let AxumQuery(query) = query.map_err(|_| {
        ApiError::Contract(
            "the query string is malformed, or carries a parameter this route does not define; \
             send only `q`, `limit`, `counts` and `view`"
                .to_string(),
        )
    })?;

    if query.q.len() > 256 {
        return Err(ApiError::Contract(format!(
            "q must be at most 256 bytes, got {}",
            query.q.len()
        )));
    }
    let limit = match query.limit {
        Some(0) => {
            return Err(ApiError::Contract(
                "limit must be at least 1".to_string(),
            ))
        }
        Some(n) => n.min(state.limits.max_suggestions),
        None => state.limits.max_suggestions,
    };
    let counts = query.counts.unwrap_or(false);

    let meta = state.engine.meta();
    let visible = session.visible_views();
    let resolved = resolve_category_column(&meta, &column, query.view.as_deref(), visible)?;

    // At most one suggestion walk per session, refused with a 429 before any work runs, so an
    // undebounced client cannot queue keystrokes. It takes no compute-gate permit; a keystroke
    // queued behind viewport renders would be useless.
    let Some(_suggest_guard) = state.suggest_admission.try_begin(session.token_id()) else {
        return Err(ApiError::Backpressure {
            retry_after_s: crate::error::RETRY_AFTER_SECS,
            cause: crate::error::ShedCause::SuggestInFlight,
        });
    };

    let walk_budget = state.limits.max_suggestion_walk;
    let max_suggest_set_entities = state.limits.max_suggest_set_entities;
    let q = query.q.clone();
    let page = state
        .blocking(move |state| {
            let _suggest_guard = _suggest_guard;
            state
                .engine
                .suggest(
                    &session,
                    &resolved,
                    &q,
                    limit,
                    counts,
                    walk_budget,
                    max_suggest_set_entities,
                )
                .map_err(map_engine_error)
        })
        .await?
        .ok_or_else(|| ApiError::Unknown("unknown category column".to_string()))?;

    Ok(Json(serde_json::json!({
        // The caller's own spelling, as `/v1/categories` echoes it.
        "column": column,
        "q": query.q,
        "values": page.values.iter().map(|v| {
            let mut value = serde_json::json!({
                "code": v.code,
                "key": v.key,
                "title": v.title,
                "match": {
                    "field": v.span.field.as_str(),
                    "start": v.span.start,
                    "len": v.span.len,
                },
            });
            // Present only when `counts=true`; never a `0` or null stand-in.
            if let Some(count) = v.count {
                value["count"] = serde_json::json!(count);
            }
            value
        }).collect::<Vec<_>>(),
        "more": page.more,
    })))
}

#[derive(Debug, Deserialize)]
struct ViewportReq {
    view: String,
    zoom: u8,
    /// Absent exactly when `tiles` is present.
    #[serde(default)]
    bbox: Option<[f64; 4]>,
    /// The depth-`zoom` Morton prefixes to answer for, in place of a bbox. A client omits tiles it
    /// already holds, and an omitted tile costs the engine nothing. Sending both is refused.
    #[serde(default)]
    tiles: Option<Vec<u64>>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    pin: Option<PinDto>,
    /// Also serve exact masked counts at depth `zoom + underlay_offset`; absent or `0` means none.
    /// Out-of-bounds values are refused, not clamped: a Morton prefix carries no depth, so a
    /// reduced offset would return cells the client could not read.
    #[serde(default)]
    underlay_offset: Option<u8>,
    /// The filter expression, parsed by [`crate::filter_dto`]; absent is unfiltered. An unknown
    /// column is a 422 and an unknown value an empty operand.
    #[serde(default)]
    filters: Option<serde_json::Value>,
    /// The annotation layers to answer for, which also sets the membership columns the points
    /// frames carry. Absent or `[]` is none and costs nothing; `"all"` is every reachable layer. A
    /// layer the principal cannot reach is absent from the answer, as an unregistered one is.
    #[serde(default)]
    layers: Option<LayersReq>,
    /// At most how many artifacts to return, met by serving ancestors in place of descendants,
    /// never by sampling. A flat layer has no ancestors, so there it has no effect.
    #[serde(default)]
    artifact_budget: Option<u32>,
    /// Which declared levels of each named layer to answer for. Absent follows each layer's zoom
    /// ranges (every level if none declares one); `"all"` is every level; `[]` is none. A layer
    /// with no levels ignores this, and a level a layer lacks is absent rather than refused.
    #[serde(default)]
    levels: Option<LevelsReq>,
    /// Which declared computed properties (`centroid`, `box`, `shape`) to serve, intersected with
    /// each layer's declaration; absent is the declaration's set and `[]` is none. A name outside
    /// the vocabulary is a 422, since the vocabulary is published schema.
    #[serde(default)]
    computed: Option<Vec<String>>,
    /// Artifact row columns: `"full"` (the default) or `"identity"`, the same rows as `layer`,
    /// `tessera_id`, `rung` and `matched` only, for a caller that already holds the payload. Rows
    /// and bits are identical either way, so the projection discloses nothing.
    #[serde(default)]
    artifact_rows: Option<ArtifactRowsReq>,
    /// The highlight expression, in `filters`' grammar. It never changes which rows are served; it
    /// adds `highlighted` to the tiles, points and artifacts frames, each counted within the
    /// filtered candidate.
    #[serde(default)]
    highlight: Option<serde_json::Value>,
    /// Point columns: `"full"` (the default) or `"highlight"`, the same points as `(tessera_id,
    /// highlighted)` for a client that changed only its highlight. A stale stamp means re-ask with
    /// `"full"`; with no `highlight` it answers as `"full"`.
    #[serde(default)]
    point_rows: Option<PointRowsReq>,
}

/// The `point_rows` field's two values; see [`ArtifactRowsReq`].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PointRowsReq {
    Full,
    Highlight,
}

/// The `artifact_rows` field's two values. An unknown value is a 422 naming both.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactRowsReq {
    Full,
    Identity,
}

/// The `layers` field: a list of names or `"all"`. Untagged, so any other string is a 422 rather
/// than a name that matches no layer.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LayersReq {
    All(AllLayers),
    Named(Vec<String>),
}

/// The `levels` field: a list of level numbers or `"all"`, untagged as [`LayersReq`] is.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LevelsReq {
    All(AllLevels),
    Named(Vec<u32>),
}

/// The literal `"all"` and only that, for `levels`.
#[derive(Debug)]
struct AllLevels;

impl<'de> Deserialize<'de> for AllLevels {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        if word == tessera_types::layer::RESERVED_LAYER_SELECTION {
            Ok(AllLevels)
        } else {
            Err(serde::de::Error::custom(format!(
                "`levels` is a list of level numbers or the string \"{}\"; got \"{word}\"",
                tessera_types::layer::RESERVED_LAYER_SELECTION
            )))
        }
    }
}

/// The literal `"all"` (`RESERVED_LAYER_SELECTION`) and only that, for `layers`.
#[derive(Debug)]
struct AllLayers;

impl<'de> Deserialize<'de> for AllLayers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        if word == tessera_types::layer::RESERVED_LAYER_SELECTION {
            Ok(AllLayers)
        } else {
            Err(serde::de::Error::custom(format!(
                "`layers` is a list of layer names or the string \"{}\"; got \"{word}\"",
                tessera_types::layer::RESERVED_LAYER_SELECTION
            )))
        }
    }
}

/// What the handler needs to build the `Response`, sent once when the engine's sweep completes.
/// Errors before then travel the same way, so they keep their status codes.
struct FirstFlush {
    coordinates: tessera_engine::ViewCoordinates,
    stamp: GenerationStamp,
    stale: bool,
    /// The `x-tessera-region` verdict; `None` when the request had no region leaf.
    region: Option<tessera_engine::RegionVerdict>,
    /// The tiles frame and, if requested, the sub-cells frame: the body's first bytes.
    first_frames: Vec<u8>,
    /// Microseconds from admission to the first flush, sent as `x-tessera-server-us`. The
    /// trailer's `stream_us` covers the whole stream.
    server_us: u64,
}

/// The engine's [`ViewportSink`]: serialises each delivery to frames, hands the first flush to the
/// handler and sends the rest into the bounded body channel. The engine gathers no entity id, so
/// none can reach these frames; scalar names come from the head, of the same generation.
struct WireSink {
    head: Option<ViewportHead>,
    /// The stream; its whole-stream deadline starts at the first flush.
    producer: Producer<FirstFlush>,
    permits: GatePermits,
    /// Taken after admission and before spawning, so blocking-pool wait counts in `server_us`.
    start: Instant,
    /// Which artifacts frame shape [`Self::artifacts`] writes; the engine computes the same rows
    /// either way.
    artifact_rows: tessera_engine::ArtifactRows,
    /// Which points frame shape [`Self::points`] writes.
    point_rows: tessera_engine::PointRows,
    arrow_serialise_ns: u64,
    points_total: u64,
    flushes: u64,
    /// Served artifacts whose shape hit its vertex budget, so a coarse drawing shows in the trace.
    shape_guard_fired: u64,
}

impl ViewportSink for WireSink {
    fn head(&mut self, head: ViewportHead) -> SinkResult {
        self.head = Some(head);
        Ok(())
    }

    fn counts(
        &mut self,
        tiles: &[tessera_engine::TileCount],
        sub_cells: Option<&[tessera_engine::SubCellCount]>,
    ) -> SinkResult {
        let serialise_start = Instant::now();
        let tile: Vec<u64> = tiles.iter().map(|t| t.tile).collect();
        let visible: Vec<u64> = tiles.iter().map(|t| t.visible).collect();
        let matched: Vec<u64> = tiles.iter().map(|t| t.matched).collect();
        let served: Vec<u64> = tiles.iter().map(|t| t.served).collect();
        let highlighted: Vec<u64> = tiles.iter().map(|t| t.highlighted).collect();
        let mut frames = tiles_frame(&tile, &visible, &matched, &served, &highlighted);
        // `Some(&[])` is a present, empty frame; `None` is no frame. The request decides
        // presence, not the result.
        if let Some(cells) = sub_cells {
            let cell: Vec<u64> = cells.iter().map(|c| c.cell).collect();
            let count: Vec<u64> = cells.iter().map(|c| c.count).collect();
            frames.extend_from_slice(&sub_cells_frame(&cell, &count));
        }
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;

        // The sweep is done, so the compute permit goes back now; the slot permit is held until
        // the producer returns.
        self.permits.release_compute();
        self.producer.start_deadline();

        let head = self.head.as_ref().expect("head precedes counts");
        let first = FirstFlush {
            coordinates: head.coordinates,
            stamp: head.stamp.clone(),
            stale: head.stale,
            region: head.region,
            first_frames: frames,
            server_us: self.start.elapsed().as_micros() as u64,
        };
        // A refusal means the client disconnected during the sweep: stop.
        self.producer.open(first)
    }

    /// Never called with an empty slice. Sent as a body frame after the first flush, so the
    /// counts are never delayed behind the artifact sweep.
    fn artifacts(&mut self, artifacts: &[tessera_engine::ArtifactOut]) -> SinkResult {
        let serialise_start = Instant::now();
        self.shape_guard_fired += artifacts.iter().filter(|a| a.shape_guard_fired).count() as u64;
        let rows: Vec<ArtifactRow<'_>> = artifacts
            .iter()
            .map(|a| ArtifactRow {
                layer: a.layer.as_str(),
                tessera_id: a.tessera_id.raw(),
                key: a.key.as_deref(),
                masked_count: a.masked_count,
                centroid: a.derived.centroid,
                bbox: a.derived.bbox,
                shape: a.derived.shape.as_deref(),
                content: &a.content,
                parent_ids: a.parent_ids.iter().map(|id| id.raw()).collect(),
                rung: a.rung,
                matched: a.matched,
                highlighted: a.highlighted,
                target: a.target.map(|id| id.raw()),
            })
            .collect();
        // The projection changes which columns are written, never which artifacts are served.
        let frame = match self.artifact_rows {
            tessera_engine::ArtifactRows::Full => artifacts_frame(&rows),
            tessera_engine::ArtifactRows::Identity => artifacts_identity_frame(&rows),
        };
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;
        self.producer.send(frame)
    }

    fn points(&mut self, chunk: tessera_engine::PointColumns) -> SinkResult {
        let head = self.head.as_ref().expect("head precedes points");
        let serialise_start = Instant::now();
        // The engine's buffers go on the wire borrowed. Names zip with the chunk's columns by
        // position; both lists hold only rendered columns, in declaration order.
        let scalar_refs: Vec<(&str, ScalarColumn)> = head
            .render_scalars
            .iter()
            .zip(&chunk.scalars)
            .map(|(d, col)| (d.name.as_str(), column_ref(col)))
            .collect();
        // Membership columns carry their own layer names, since the artifact pass settles them
        // after the head.
        let membership_refs: Vec<(&str, &[Option<u64>])> = chunk
            .membership
            .iter()
            .map(|m| (m.layer.as_str(), m.ids.as_slice()))
            .collect();
        // The highlight projection writes the two-column frame; the rows are the same either way.
        let frame = match (self.point_rows, chunk.highlighted.as_deref()) {
            (tessera_engine::PointRows::Highlight, Some(bits)) => {
                points_highlight_frame(&chunk.tessera_ids, bits)
            }
            (_, bits) => points_frame(
                &chunk.tessera_ids,
                &chunk.codes,
                &scalar_refs,
                bits,
                &membership_refs,
            ),
        };
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;
        self.points_total += chunk.tessera_ids.len() as u64;
        self.flushes += 1;
        self.producer.send(frame)
    }
}

/// What a request's filter expressions are parsed against: one `Engine::meta()` snapshot and the
/// view the request names, whose frame and projection a `region` leaf is canonicalised in.
pub(crate) struct FilterParser<'a> {
    meta: &'a tessera_engine::EngineMeta,
    view: &'a tessera_engine::MetaView,
    visible: &'a tessera_engine::gate::VisibleViews,
    region: crate::filter_dto::RegionContext,
    /// Each category's vocabulary by the leaf's bare name, entity-scoped columns and group-scoped
    /// families alike. Names are unique across the two lists, so one map cannot answer two things.
    vocab_of: std::cell::OnceCell<std::collections::HashMap<&'a str, &'a str>>,
}

impl<'a> FilterParser<'a> {
    pub(crate) fn new(
        meta: &'a tessera_engine::EngineMeta,
        view: &'a tessera_engine::MetaView,
        visible: &'a tessera_engine::gate::VisibleViews,
        max_region_vertices: u64,
    ) -> Self {
        FilterParser {
            meta,
            view,
            visible,
            region: crate::filter_dto::RegionContext {
                extent: crate::filter_dto::view_extent(view),
                projection: view.projection,
                max_vertices: max_region_vertices,
            },
            vocab_of: std::cell::OnceCell::new(),
        }
    }

    pub(crate) fn parse(
        &self,
        value: &serde_json::Value,
    ) -> Result<tessera_engine::filter::FilterExpr, ApiError> {
        let meta = self.meta;
        let vocab_of = self.vocab_of.get_or_init(|| {
            meta.declared_scalars
                .iter()
                .filter_map(|d| Some((d.name.as_str(), d.vocabulary.as_deref()?)))
                .chain(
                    meta.scoped_scalars
                        .iter()
                        .filter_map(|f| Some((f.name.as_str(), f.vocabulary.as_deref()?))),
                )
                .collect()
        });
        crate::filter_dto::parse(
            value,
            &|leaf| meta.resolve_filter_column(leaf, &self.view.id, self.visible),
            &|column, key| {
                // A scoped family's pin decides which column is read, never which value set the
                // key is in, so it is dropped before the lookup.
                let name = column
                    .split_once(tessera_engine::filter::PIN)
                    .map_or(column, |(name, _)| name);
                let vocabulary = vocab_of.get(name)?;
                meta.vocabularies.get(vocabulary)?.code_of(key)
            },
            &self.region,
        )
    }
}

/// The producer: the engine call and frame serialisation, run on a blocking thread for the whole
/// stream. It answers the handler once (an error before the first flush, or the first flush) and
/// then sends frames; the permits release when `sink` drops at the end.
fn run_viewport_stream(
    state: &AppState,
    session: &tessera_engine::Session,
    req: ViewportReq,
    cancel: CancelToken,
    mut sink: WireSink,
) {
    let stamp = req.pin.map(GenerationStamp::from);
    // One `Engine::meta()` for the whole request, so the view and the filter parse read the same
    // generation.
    let meta = state.engine.meta();
    // An unknown key, an undeclared name and a view the principal cannot reach all get the same
    // 404, at the same cost.
    let Some(view) = meta.resolve_visible_view(&req.view, session.visible_views()) else {
        sink.producer
            .refuse(ApiError::Unknown(format!("unknown view '{}'", req.view)));
        return;
    };
    let view_id = view.id.clone();
    // The default `k` is the deployment's overplot ceiling, so a client with no preference gets
    // the full density range.
    let k = req
        .k
        .unwrap_or_else(|| state.engine.config().k_max_marks)
        .min(state.limits.max_k);

    // Deduplicated, keeping the first occurrence and the caller's order: a repeated tile would be
    // counted twice, and the response follows the request's order so a client can ask centre-out.
    let tiles = req.tiles.map(|list| {
        let mut seen = std::collections::HashSet::with_capacity(list.len());
        list.into_iter()
            .filter(|t| seen.insert(*t))
            .collect::<Vec<_>>()
    });
    // Validated as present-and-alone by the handler before admission; the default here is inert.
    let bbox = req.bbox.unwrap_or([0.0, 0.0, 0.0, 0.0]);

    // Both expressions are parsed against one schema before any compute and before the first
    // frame, while a refusal can still be a 422 rather than a truncated stream.
    let parser = FilterParser::new(
        &meta,
        view,
        session.visible_views(),
        state.limits.max_region_vertices,
    );
    // Refusals go to the handler, which is still waiting, so they keep their status.
    let mut refuse = |e| sink.producer.refuse(e);
    let filter = match req.filters.as_ref().map(|v| parser.parse(v)) {
        None => None,
        Some(Ok(expr)) => Some(expr),
        Some(Err(e)) => {
            refuse(e);
            return;
        }
    };
    let highlight = match req.highlight.as_ref().map(|v| parser.parse(v)) {
        None => None,
        Some(Ok(expr)) => Some(expr),
        Some(Err(e)) => {
            refuse(e);
            return;
        }
    };

    // Owned copies for the shed log: the resolved view id, the zoom and the caller's layer names.
    // No principal is logged.
    let named_view = view_id.clone();
    let named_zoom = req.zoom;
    let named_layers = match &req.layers {
        Some(LayersReq::All(_)) => tessera_types::layer::RESERVED_LAYER_SELECTION.to_string(),
        Some(LayersReq::Named(names)) => names.join(","),
        None => String::new(),
    };

    // Omitted means no layers.
    let layer_names: Vec<&str> = match &req.layers {
        Some(LayersReq::Named(names)) => names.iter().map(String::as_str).collect(),
        Some(LayersReq::All(_)) | None => Vec::new(),
    };
    let layers = match &req.layers {
        Some(LayersReq::All(_)) => LayerSelection::All,
        Some(LayersReq::Named(_)) | None => LayerSelection::Named(&layer_names),
    };
    // Omitted follows each layer's declared zoom ranges, unlike `layers`: naming a layer already
    // opted in to the artifact pass. Every level, the costly answer, must be asked for.
    let level_numbers: Vec<u32> = match &req.levels {
        Some(LevelsReq::Named(levels)) => levels.clone(),
        Some(LevelsReq::All(_)) | None => Vec::new(),
    };
    let levels = match &req.levels {
        Some(LevelsReq::All(_)) => LevelSelection::All,
        Some(LevelsReq::Named(_)) => LevelSelection::Named(&level_numbers),
        None => LevelSelection::Declared,
    };
    // Omitted is the declaration's own set. Unknown names were refused by the handler, so the
    // `filter_map` drops none.
    let computed_named: Vec<tessera_engine::ComputedProperty> = req
        .computed
        .iter()
        .flatten()
        .filter_map(|name| tessera_engine::ComputedProperty::parse_ask(name))
        .collect();
    let computed = match &req.computed {
        Some(_) => ComputedSelection::Named(&computed_named),
        None => ComputedSelection::Declared,
    };
    // Omitted is `"full"`. Both the engine (which then skips payloads) and the sink (which writes
    // the four-column frame) are told; the rows are the same either way.
    let artifact_rows = match req.artifact_rows {
        Some(ArtifactRowsReq::Identity) => tessera_engine::ArtifactRows::Identity,
        Some(ArtifactRowsReq::Full) | None => tessera_engine::ArtifactRows::Full,
    };
    sink.artifact_rows = artifact_rows;
    // The same shape one field over: omitted is `"full"`, and the projection is the opt-in.
    let point_rows = match req.point_rows {
        Some(PointRowsReq::Highlight) => tessera_engine::PointRows::Highlight,
        Some(PointRowsReq::Full) | None => tessera_engine::PointRows::Full,
    };
    sink.point_rows = point_rows;
    let mut request = ViewportRequest::new(&view_id, req.zoom, bbox, k)
        .tiles(tiles.as_deref())
        .stamp(stamp)
        .underlay_offset(req.underlay_offset)
        .layers(layers)
        .artifact_budget(req.artifact_budget)
        .levels(levels)
        .computed(computed)
        .artifact_rows(artifact_rows)
        .point_rows(point_rows)
        .cancel(Some(cancel));
    if let Some(filter) = filter {
        request = request.filter(filter);
    }
    if let Some(highlight) = highlight {
        request = request.highlight(highlight);
    }

    let outcome =
        state
            .engine
            .viewport_stream(session, request, state.limits.stream_flush_bytes, &mut sink);

    match outcome {
        Ok(timings) => {
            // Exactly these keys; the conformance comparator checks them. `stream_us` includes
            // client-paced waits, so it is not the server-cost figure (`x-tessera-server-us`).
            let mut trailer = serde_json::json!({
                "stream_us": sink.start.elapsed().as_micros() as u64,
                "arrow_serialise_ns": sink.arrow_serialise_ns,
                "points": sink.points_total,
                "flushes": sink.flushes,
            });
            if state.limits.stage_timing {
                if let Some(csv) =
                    stage_header(&timings, sink.arrow_serialise_ns, sink.shape_guard_fired)
                {
                    trailer["stage_ns"] = serde_json::Value::String(csv);
                }
            }
            sink.producer
                .finish(trailer_frame(trailer.to_string().as_bytes()));
        }
        // Before the first flush nothing is committed and the handler is waiting, so the error
        // keeps its status; a cancellation here is a 500 nobody reads.
        Err(e) if !sink.producer.is_open() => sink.producer.refuse(map_engine_error(e)),
        // Mid-body the 200 is committed: no trailer is sent and the body aborts the transport. A
        // client disconnect logs nothing; anything else is logged.
        Err(e) => {
            // A server-chosen shed is logged apart from a client disconnect. The elapsed time is
            // the diagnosis: just past the deadline is a reader that stopped, a multiple of it is
            // work behind the next frame.
            if let Some(shed) = sink.producer.shed() {
                tracing::warn!(
                    view = %named_view,
                    zoom = named_zoom,
                    layers = %named_layers,
                    elapsed_ms = sink.start.elapsed().as_millis() as u64,
                    since_first_flush_ms = sink
                        .producer
                        .deadline_from()
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0),
                    deadline_ms = sink.producer.deadline().map_or(0, |d| d.as_millis() as u64),
                    stall_ms = sink.producer.stall().as_millis() as u64,
                    flushes = sink.flushes,
                    "viewport stream SHED mid-body by the server — {}",
                    shed.detail()
                );
            } else if !matches!(e, tessera_engine::EngineError::Cancelled) {
                tracing::warn!(error = %e, "viewport stream aborted mid-body");
            }
            sink.producer.abort();
        }
    }
    // `sink` drops here, after the state stores, releasing the channel sender and the slot permit.
}

async fn viewport(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    ApiJson(req): ApiJson<ViewportReq>,
) -> Result<Response, ApiError> {
    if req.zoom > 16 {
        return Err(ApiError::Contract("zoom must be in 0..=16".to_string()));
    }

    // Exactly one of `bbox` and `tiles`.
    match (&req.bbox, &req.tiles) {
        (Some(bbox), None) => {
            if bbox.iter().any(|v| !v.is_finite()) || bbox[0] > bbox[2] || bbox[1] > bbox[3] {
                return Err(ApiError::Contract(
                    "bbox must be [x0, y0, x1, y1] with x0 <= x1, y0 <= y1, all finite".to_string(),
                ));
            }
        }
        (None, Some(tiles)) => {
            // A prefix carries no depth, so one with bits above `zoom` is refused rather than
            // masked off.
            let shift = 2 * u32::from(req.zoom);
            if let Some(bad) = tiles
                .iter()
                .find(|&&prefix| shift < 64 && prefix >> shift != 0)
            {
                return Err(ApiError::Contract(format!(
                    "tile prefix {bad} has bits above depth {}",
                    req.zoom
                )));
            }
        }
        (Some(_), Some(_)) => {
            return Err(ApiError::Contract(
                "send exactly one of bbox and tiles, not both".to_string(),
            ));
        }
        (None, None) => {
            return Err(ApiError::Contract(
                "send exactly one of bbox and tiles".to_string(),
            ));
        }
    }

    // An unknown computed-property name is refused before admission; the vocabulary is
    // published schema, so the refusal discloses nothing.
    if let Some(names) = &req.computed {
        if let Some(bad) = names
            .iter()
            .find(|name| tessera_engine::ComputedProperty::parse_ask(name).is_none())
        {
            return Err(ApiError::Contract(format!(
                "`computed` names {bad:?}; the computed properties are {}",
                tessera_engine::ComputedProperty::ASK_VOCABULARY.join(", ")
            )));
        }
    }

    // Created before admission so one token covers the whole request; the guard then moves into
    // the response body.
    let cancel = CancelToken::new();
    let cancel_guard = CancelGuard::new(cancel.clone());

    // Two-stage admission: 429 if no slot is free, or if no compute permit frees within
    // `admission_timeout_ms`. `admission_us` is the queue wait.
    let (gate_permits, admission_us) = state.compute_gate.admit().await?;

    // `x-tessera-server-us` runs from after admission to the first flush, so it excludes queueing
    // and client-paced sends (the trailer's `stream_us` has those). Blocking-pool scheduling
    // counts in it.
    let start = Instant::now();

    // The producer runs detached on the blocking pool for the whole stream. The handler awaits
    // only the first flush; a producer panic shows as a 500 or, later, as a body abort. Only a
    // clone of `cancel` moves in; the guard keeps the original.
    let (producer, pending) = crate::stream::channel(
        cancel_guard,
        Duration::from_millis(state.limits.stream_write_stall_ms),
        Some(Duration::from_millis(state.limits.stream_deadline_ms)),
    );
    let sink = WireSink {
        head: None,
        producer,
        permits: gate_permits,
        start,
        // Set from the request inside the producer.
        artifact_rows: tessera_engine::ArtifactRows::Full,
        point_rows: tessera_engine::PointRows::Full,
        arrow_serialise_ns: 0,
        shape_guard_fired: 0,
        points_total: 0,
        flushes: 0,
    };
    let closure_state = Arc::clone(&state);
    let closure_cancel = cancel.clone();
    drop(tokio::task::spawn_blocking(move || {
        run_viewport_stream(&closure_state, &session, req, closure_cancel, sink);
    }));

    // Every failure before the first flush arrives here with its status.
    let (first, body) = pending.opened("viewport", "first flush").await?;

    let pin_header = serde_json::to_string(&PinDto::from(&first.stamp))
        .expect("PinDto serialisation cannot fail");

    let response = crate::stream::response_head(
        Some(&first.coordinates.identity_key),
        first.server_us,
        admission_us,
        first.region,
    )
    // The content coordinate, as an entity tag compared exactly. `If-Match` is not read, so a
    // mismatch never produces a 412. It moves separately from the identity key.
    .header(
        "etag",
        format!("\"{}\"", crate::stream::hex16(&first.coordinates.content_key)),
    )
    .header("x-tessera-pin", pin_header)
    // Always present, so a client reading only counts sees staleness without decoding a batch.
    .header("x-tessera-stale", if first.stale { "1" } else { "0" });

    // Stage timings ride the trailer frame, since a header cannot follow the body it describes.
    Ok(response
        .body(body.into_body(first.first_frames))
        .expect("response construction cannot fail"))
}

/// The trailer's `stage_ns`: a fixed-order CSV of durations and counts, `None` without the
/// `bench-timing` feature. Per-tile durations are summed across sweep workers, so above the
/// serial fallback they can exceed wall time.
#[cfg(feature = "bench-timing")]
fn stage_header(
    t: &tessera_engine::StageTimings,
    arrow_serialise_ns: u64,
    shape_guard_fired: u64,
) -> Option<String> {
    // Append only: consumers read this CSV by position.
    Some(format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        t.generation_resolve_ns,
        t.stamp_compare_ns,
        t.view_lookup_ns,
        t.row_projection_ns,
        t.compose_ns,
        t.tiles_for_bbox_ns,
        t.tile_ranges_ns,
        t.count_ns,
        t.select_ns,
        t.gather_ns,
        arrow_serialise_ns,
        t.total_ns,
        t.tiles_resolved,
        t.tiles_nonempty,
        t.sigma_visible,
        t.rows_in_ranges,
        t.select_rows_visited,
        t.points_gathered,
        u64::from(t.row_projection_built),
        t.theta_anchor_ns,
        t.underlay_ns,
        t.underlay_cells_evaluated,
        // Served artifacts whose shape hit its vertex budget.
        shape_guard_fired,
        // The walk that resolves the occupied-tile count, the one part of θ that scales with the
        // corpus.
        t.theta_occupancy_ns,
    ))
}

#[cfg(not(feature = "bench-timing"))]
fn stage_header(
    _t: &tessera_engine::StageTimings,
    _arrow_serialise_ns: u64,
    _shape_guard_fired: u64,
) -> Option<String> {
    None
}

/// The scalar families, listed once so the two matches over them cannot disagree.
macro_rules! scalar_families {
    ($mac:ident) => {
        $mac! {
            Bool, U8, U16, U32, U64, I8, I16, I32, I64, F32, F64, TimestampUs,
        }
    };
}

/// Borrows one of the engine's column-major gathered columns as the wire's view of it, uncopied.
fn column_ref(buf: &tessera_engine::ColumnBuf) -> ScalarColumn<'_> {
    use tessera_engine::ColumnBuf;
    macro_rules! arms {
        ($($v:ident),* $(,)?) => {
            match buf {
                $(ColumnBuf::$v(x) => ScalarColumn::$v(x),)*
                ColumnBuf::Utf8(x) => ScalarColumn::Utf8(x),
            }
        };
    }
    scalar_families!(arms)
}

#[derive(Debug, Deserialize)]
struct ItemReq {
    #[allow(dead_code)]
    #[serde(default)]
    pin: Option<PinDto>,
    /// Optional: the durable identifier is `external_id`. A caller that omits it accepts that a
    /// `tessera_id` from a past idset may name a different item after repartitioning.
    #[serde(default)]
    idset: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ItemResp {
    /// The full record by declared column name, a category as its vocabulary key. An absent field
    /// is omitted, never `null`.
    fields: serde_json::Map<String, serde_json::Value>,
    /// Base64, present only when the item has an external id. This is the only place one appears
    /// on the viewer plane, so the conformance byte-scanner must exclude this response.
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
    /// The item's labels that this session satisfies, sorted; never the full set, so a viewer
    /// learns no compartment they do not hold. Always present, even empty.
    labels: Vec<String>,
    /// The views containing this item that this principal may reach, sorted by id, each with the
    /// item's position. An unreachable view is absent as an undeclared one is; an item in no view
    /// is a 404, so `[]` means only that none is reachable.
    views: Vec<ItemViewDto>,
    /// Group-scoped values by family, then by the group's key: `{"mood": {"2026-Q1": "calm"}}`.
    /// Keys the principal cannot reach are omitted; a family with no reachable value is absent.
    scoped: serde_json::Map<String, serde_json::Value>,
}

/// One entry of [`ItemResp::views`]: a view this item is in, and where that view puts it.
#[derive(Debug, Serialize)]
struct ItemViewDto {
    id: String,
    /// The position in this view's 32-bit grid units, which the viewport's Morton codes decode
    /// to. Sent as two axes because JSON cannot carry the 64-bit code exactly.
    x: u32,
    y: u32,
}

/// The blocking part of `/v1/items/{tessera_id}`: the engine lookup and the shaping after it.
/// "No such id" and "not visible to you" are one 404 from one site, with no logging on either, so
/// a viewer cannot tell them apart. The engine tests visibility before any file read, so a store
/// failure (a 500) can arise only for an item the viewer can already see. The idset check (a 409)
/// is the same work for every id.
fn run_item(
    state: &AppState,
    session: &tessera_engine::Session,
    raw: u64,
    idset: Option<u32>,
) -> Result<ItemResp, ApiError> {
    let item = match state.engine.item(session, TesseraId::new(raw), idset) {
        // A stale idset is a 409; a store or IO failure is a 500, never a missing field.
        Err(e) => return Err(map_engine_error(e)),
        // The one 404, for no such id and for not visible alike.
        Ok(None) => return Err(ApiError::Unknown("unknown".to_string())),
        Ok(Some(item)) => item,
    };

    let fields = item
        .fields
        .into_iter()
        // Every width becomes a JSON number; `/v1/meta` publishes the storage type.
        .map(|f| (f.name, scalar_out_json(f.value)))
        .collect();

    let external_id = item
        .external_id
        .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));

        // The engine filtered both by the session's visible views; this only renames fields.
    let views = item
        .views
        .into_iter()
        .map(|v| ItemViewDto {
            id: v.id,
            x: v.x,
            y: v.y,
        })
        .collect();
    let scoped = item
        .scoped
        .into_iter()
        .map(|family| {
            let values: serde_json::Map<String, serde_json::Value> = family
                .values
                .into_iter()
                .map(|(key, value)| (key, scalar_out_json(value)))
                .collect();
            (family.name, serde_json::Value::Object(values))
        })
        .collect();

    Ok(ItemResp {
        fields,
        external_id,
        // The engine intersects these with the session's satisfied terms.
        labels: item.labels,
        views,
        scoped,
    })
}

/// One drill-down value as JSON, every width as a number. Shared by record fields and scoped
/// values so the two present a value alike.
fn scalar_out_json(value: tessera_engine::ScalarOut) -> serde_json::Value {
    macro_rules! arms {
        ($($v:ident),* $(,)?) => {
            match value {
                $(tessera_engine::ScalarOut::$v(v) => serde_json::json!(v),)*
                tessera_engine::ScalarOut::Utf8(v) => serde_json::json!(v),
            }
        };
    }
    scalar_families!(arms)
}

#[derive(Debug, Deserialize)]
struct ArtifactReq {
    /// The view whose row space the count is taken in. Required, since a masked count is per view
    /// while a record is not.
    view: String,
    /// Optional, on [`ItemReq::idset`]'s argument.
    #[serde(default)]
    idset: Option<u32>,
    /// The depth the caller draws at: a predicate or authored shape then omits vertices that move
    /// an edge by under a pixel. Absent serves the whole presimplified shape; a hull ignores it.
    #[serde(default)]
    zoom: Option<u8>,
}

#[derive(Debug, Serialize)]
struct ArtifactResp {
    layer: String,
    /// The publisher's own key, if they supplied one. Absent rather than `null` when they did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// How many of the artifact's members this principal can see, never how many it has.
    masked_count: u64,
    /// The layer's declared derived geometry, recomputed for this principal, in viewport grid
    /// units. Absent only where the layer declares none; an unservable artifact is a 404.
    #[serde(skip_serializing_if = "Option::is_none")]
    centroid: Option<[f64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#box: Option<[u32; 4]>,
    /// The artifact's drawn geometry as parts, rings, vertices; a part's first ring is its outer
    /// and the rest are holes. Its kind is the layer's, as `/v1/meta` publishes. Absent if none.
    #[serde(skip_serializing_if = "Option::is_none")]
    shape: Option<Vec<Vec<Vec<[u32; 2]>>>>,
    /// The declared level on a levelled layer, `0` otherwise. Unlike the rest, it is the same for
    /// every principal.
    rung: u32,
    /// The supplied content, whole and positional to the layer's declared kinds; empty if it
    /// declares none. Content this principal may not read makes the artifact a 404.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    content: Vec<String>,
}

/// `POST /v1/artifacts/browse`: one JSON page of a layer's hierarchy (roots, children or search).
/// Deployment-schema mistakes are 422s; a `parent` this principal is not served answers an empty
/// page, as a `member_of` leaf does. Runs on the compute gate, as `/v1/viewport` does.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowseReq {
    /// The view whose row space the counts are taken in; required as `ArtifactReq::view` is.
    view: String,
    /// The layer to browse. A name outside `/v1/meta`'s list is a `422`.
    layer: String,
    /// The level to address on a levelled layer (`stacked`, `tiered`). A 422 on `flat`, `nested`
    /// and `dag`, which have one level.
    #[serde(default)]
    level: Option<u32>,
    /// The children form: the artifacts naming this one among their parents, with its own served
    /// parents in `parents`. A `tessera_id`, as a number or its decimal string.
    #[serde(default)]
    parent: Option<serde_json::Value>,
    /// The search form: the layer's artifacts whose key, or whose first supplied text content,
    /// contains this case-insensitively.
    #[serde(default)]
    q: Option<String>,
    /// The viewport's filter object. Rows then carry `matched_count` and are ordered by it;
    /// existence and `masked_count` never change with it.
    #[serde(default)]
    filters: Option<serde_json::Value>,
    /// Page size, clamped to `selection.max_browse_rows`. `0` is a `422`.
    #[serde(default)]
    limit: Option<usize>,
    /// The `next` of a previous page.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct BrowseResp {
    artifacts: Vec<BrowseRowResp>,
    /// The requested artifact's own served parents; `[]` except in the children form.
    parents: Vec<BrowseRowResp>,
    /// The cursor for the next page, absent where this page is the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

#[derive(Debug, Serialize)]
struct BrowseRowResp {
    /// A string, since a `u64` does not survive a JavaScript number intact.
    tessera_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    masked_count: u64,
    /// Present exactly when the request carried `filters`.
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_count: Option<u64>,
    rung: u32,
    /// This artifact's parents that this principal is also served.
    parent_ids: Vec<String>,
}

fn browse_row(row: tessera_engine::browse::BrowseRow) -> BrowseRowResp {
    BrowseRowResp {
        tessera_id: row.tessera_id.raw().to_string(),
        key: row.key,
        name: row.name,
        masked_count: row.masked_count,
        matched_count: row.matched_count,
        rung: row.rung,
        parent_ids: row
            .parent_ids
            .iter()
            .map(|id| id.raw().to_string())
            .collect(),
    }
}

async fn browse(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    ApiJson(req): ApiJson<BrowseReq>,
) -> Result<Json<BrowseResp>, ApiError> {
    use tessera_engine::browse::{BrowseCursor, BrowseForm, BrowseRequest};
    // `limit` clamps and `0` refuses, as on `/v1/categories`.
    if req.limit == Some(0) {
        return Err(ApiError::Contract(
            "`limit` is 0; omit it for the deployment's default, or send a positive number up \
             to `selection.max_browse_rows`"
                .to_string(),
        ));
    }
    let limit = req
        .limit
        .map_or(state.limits.max_browse_rows, |n| n.min(state.limits.max_browse_rows));
    // At most one of `parent` and `q`: roots, children or search.
    if req.parent.is_some() && req.q.is_some() {
        return Err(ApiError::Contract(
            "`parent` and `q` are two forms of this verb, children and search; send at most \
             one of them"
                .to_string(),
        ));
    }
    let form = match (&req.parent, &req.q) {
        (Some(value), _) => {
            BrowseForm::Children(crate::filter_dto::tessera_id(Some(value), "parent")?)
        }
        (None, Some(q)) => BrowseForm::Search(q.clone()),
        (None, None) => BrowseForm::Roots,
    };
    let cursor = match &req.cursor {
        None => None,
        Some(text) => Some(BrowseCursor::parse(text).ok_or_else(|| {
            ApiError::Contract(
                "`cursor` is not one this endpoint issued; pass back a page's `next` unchanged"
                    .to_string(),
            )
        })?),
    };
    let out = state
        .gated(move |state| {
            let meta = state.engine.meta();
            // The same view resolution every other viewer verb takes, gate included.
            let view = meta
                .resolve_visible_view(&req.view, session.visible_views())
                .ok_or_else(|| ApiError::Unknown(format!("unknown view '{}'", req.view)))?;
            // Parsed before any compute, by the viewport's parser, so one filter means the same
            // on both.
            let filter = req
                .filters
                .as_ref()
                .map(|value| {
                    FilterParser::new(
                        &meta,
                        view,
                        session.visible_views(),
                        state.limits.max_region_vertices,
                    )
                    .parse(value)
                })
                .transpose()?;
            state
                .engine
                .browse(
                    &session,
                    BrowseRequest {
                        view: &view.id,
                        layer: &req.layer,
                        level: req.level,
                        form,
                        filter,
                        limit,
                        cursor,
                    },
                )
                .map_err(crate::error::map_engine_error)
        })
        .await?;
    Ok(Json(BrowseResp {
        artifacts: out.artifacts.into_iter().map(browse_row).collect(),
        parents: out.parents.into_iter().map(browse_row).collect(),
        next: out.next.map(|c| c.encode()),
    }))
}

/// `POST /v1/artifacts/{tessera_id}`: one artifact's drill-down. A separate route from
/// `/v1/items`, so the shape of an answer never says which kind an id names. An id naming nothing
/// or a point and an artifact not visible to this principal (unreachable layer, suppressed, below
/// its existence criterion) all get one 404 with one detail, so a viewer cannot tell them apart.
/// Runs on the compute gate, like `/v1/viewport`.
async fn artifact(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    AxumPath(raw): AxumPath<u64>,
    ApiJson(req): ApiJson<ArtifactReq>,
) -> Result<Json<ArtifactResp>, ApiError> {
    let served = state
        .gated(move |state| {
            // The viewport's view resolution, gate included. A view that goes away before the
            // engine call is the same unknown-view 404.
            let view = state
                .engine
                .meta()
                .resolve_visible_view(&req.view, session.visible_views())
                .map(|v| v.id.clone())
                .ok_or_else(|| ApiError::Unknown(format!("unknown view '{}'", req.view)))?;
            state
                .engine
                .artifact(
                    &session,
                    TesseraId::new(raw),
                    req.idset,
                    &view,
                    req.zoom,
                )
                .map_err(crate::error::map_engine_error)
        })
        .await?;

    // One construction site and one detail for every withheld case; see the doc above.
    let served = served.ok_or_else(|| ApiError::Unknown("unknown artifact".to_string()))?;
    Ok(Json(ArtifactResp {
        layer: served.layer,
        key: served.key,
        masked_count: served.masked_count,
        centroid: served.derived.centroid,
        r#box: served.derived.bbox,
        shape: served.derived.shape,
        content: served.content,
        rung: served.rung,
    }))
}

async fn item(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    AxumPath(raw): AxumPath<u64>,
    ApiJson(req): ApiJson<ItemReq>,
) -> Result<Json<ItemResp>, ApiError> {
    // `Engine::item` checks the idset against the one generation it loads, so a stale-idset
    // request holds a gate permit rather than being refused before admission.
    let resp = state
        .gated(move |state| run_item(state, &session, raw, req.idset))
        .await?;

    Ok(Json(resp))
}
