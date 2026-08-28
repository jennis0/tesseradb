//! The viewer plane (R5): `GET /v1/meta`, `POST /v1/viewport`, `POST /v1/items/{tessera_id}`,
//! `POST /v1/artifacts/{tessera_id}`, plus `/healthz`/`/readyz`. Bearer auth is a session token
//! (`Session::token`), minted by the session plane's `/session/authorise`.

use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use tessera_types::{GenerationStamp, TesseraId};
use tessera_wire::{
    artifacts_frame, points_frame, sub_cells_frame, tiles_frame, trailer_frame, ArtifactRow,
    ScalarColumn,
};

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{
    CancelToken, LayerSelection, LevelSelection, SinkClosed, SinkResult, ViewportHead, ViewportSink,
};

use crate::error::{map_engine_error, map_join_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::{AppState, GatePermits};

pub fn router(state: Arc<AppState>) -> Router {
    // Both browser seams land here and only here: `serve.dev_cors_origins` (development) and
    // `serve.cors_origins` (production token presentation, decision 0102). With neither set the
    // layer is `None` and the seam is structurally absent from this router rather than present
    // and configured empty.
    let cors = crate::cors::viewer_layer(&state);
    let router = Router::new()
        .route("/v1/meta", get(meta))
        .route("/v1/categories/{column}", get(categories))
        .route("/v1/viewport", post(viewport))
        .route("/v1/items/{tessera_id}", post(item))
        .route("/v1/artifacts/{tessera_id}", post(artifact))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state);
    match cors {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

/// The wire shape of a generation stamp, both in `POST /v1/viewport`'s request body and the
/// `x-tessera-pin` response header (JSON either way — the header carries the same shape as a plain
/// string so a client can round-trip it without inventing its own encoding).
///
/// **The header keeps its name and loses its meaning** (contracts §3.1, §3.2). It used to select
/// geometry: presenting a superseded one was a `410 pin-expired`, and the server retained
/// superseded generations so it could be honoured. It is now a stamp — the client echoes back what
/// it is holding, the server answers from live geometry regardless, and the only effect is the
/// `stale` flag on the response (`geometry-pinning.md` §7). The name is kept because renaming a
/// header is a client-visible break that buys nothing; the meaning change is in the contract.
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

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Flips a [`CancelToken`] on drop. Created before the admission-gate acquire, held by the
/// `viewport` handler while it awaits the first flush, then moved into the [`StreamBody`] for
/// the life of the response — so the one transport signal for "the client went away" (the
/// future, and later the body, being dropped) flips the flag the `spawn_blocking` producer's
/// engine call is polling, at whichever phase the disconnect lands. Only a *clone* of the token
/// moves into that closure; this guard keeps the original.
///
/// **Disarmed on clean completion only** — [`StreamBody`] disarms when the channel ends with
/// the trailer sent, so a completed stream's guard drop does not flip a token nobody is reading
/// any more. Flipping it late would in fact be harmless — the producer has already returned on
/// that path — but disarming keeps "cancelled" meaning what it says: this request was cut
/// short, not merely finished.
struct CancelGuard {
    token: CancelToken,
    armed: bool,
}

impl CancelGuard {
    fn new(token: CancelToken) -> Self {
        CancelGuard { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

async fn meta(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Authenticated like every other route on this plane (R5: bearer auth on every plane). It is
    // not a public endpoint: it discloses the bundle's extents, views and declared-scalar schema,
    // so an unauthenticated `/v1/meta` would hand the corpus shape to anyone who can reach the
    // viewer listener. The bearer here is a session token, so a valid, unexpired session is
    // required exactly as for `/v1/viewport`.
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    let meta = state.engine.meta();
    let selection = state.engine.config();
    // **Gate-filtered per principal, and this is the only per-principal field on the document.**
    // Everything else here is a deployment constant identical for every caller; the layer list is
    // not, and a shared cache over this response would hand one principal another's registry. The
    // filtering is two steps in the engine — reachability by terms, then a live suppression check
    // on each layer's own entity — so a layer this caller may not know about is absent by the same
    // route a name nobody registered is.
    let layers = state.engine.visible_layers(&entry.session);
    Ok(Json(serde_json::json!({
        "api_version": meta.api_version,
        "bundle_format": meta.bundle_format,
        // contracts §2.2/§2.6: the idset. The identity KEY never appears
        // in any response, log line or metric label (I10, Appendix C C17) -- this is the idset
        // only, which is meaningless without the key and is what `POST /v1/items` checks against.
        "idset": meta.idset,
        "views": meta.views.iter().map(|(id, name)| serde_json::json!({"id": id, "display_name": name})).collect::<Vec<_>>(),
        "quantisation": {
            "x_min": meta.quantisation.x_min,
            "x_max": meta.quantisation.x_max,
            "y_min": meta.quantisation.y_min,
            "y_max": meta.quantisation.y_max,
        },
        // The column schema, and the **whole** of it: name, storage type, and — for a category —
        // the vocabulary it draws from, that vocabulary's kind and its `visibility`. Without the
        // `category` block a client cannot tell a `u16` category from a `u16` integer, since the
        // hot path ships the code and nothing else.
        //
        // **The name is the column's identifier**, here and in `/v1/categories/{column}`. It is
        // unique bundle-wide (`tessera_build::config` refuses a duplicate) and restricted to a
        // path-safe character set for that reason, so no second identifier is minted for it.
        //
        // **Values are not here.** A large vocabulary is megabytes against a measured 79 KB
        // viewport response, and `derived` filtering means no shared cache — so values are a
        // separate, paged, per-principal endpoint and this stays a small shared document
        // (per-point-attributes §3.8).
        "declared_scalars": meta.declared_scalars.iter().map(|s| {
            let category = s.vocabulary.as_deref().and_then(|name| {
                let vocabulary = meta.vocabularies.get(name)?;
                Some(serde_json::json!({
                    "vocabulary": name,
                    "kind": match vocabulary.kind() {
                        tessera_engine::VocabularyKind::Declared => "declared",
                        tessera_engine::VocabularyKind::Discovered => "discovered",
                    },
                    "visibility": vocabulary.visibility().as_str(),
                }))
            });
            serde_json::json!({
                "name": s.name,
                "arrow_type": s.arrow_type.arrow_type_name(),
                "category": category,
                // **The analyser that produced a `text` column's terms**, as the full
                // `<name>/<version>` identity the manifest records (decision 0070); `null` for
                // every other type, which genuinely has none.
                //
                // The wire carries query text unanalysed and the server segments it, which is the
                // right split — a client cannot reproduce a pipeline it cannot see. The cost is
                // that an empty answer is ambiguous: *no document says this* and *your query
                // segmented differently from the index* look identical, and the second is the
                // likely one for CJK or Thai. The identity is what separates them — `tessera
                // tokenise --analyser <identity's name> --identity` reproduces the segmentation
                // locally — so withholding it leaves a client with a dead end rather than a
                // diagnosis.
                //
                // It discloses nothing: deployment schema, identical for every principal, the same
                // class as `arrow_type` and `family` beside it.
                "analyser": s.analyser,
                // The column's compiled placement (records §3): `render` — a slot in every row of
                // the hot column; `index` — an entity-space search structure. Neither set means
                // blob-resident: stored, returned at drill-down, not filterable — derived from
                // the two flags exactly as the manifest derives it, never a third flag.
                "render": s.render,
                "index": s.index,
            })
        }).collect::<Vec<_>>(),
        // Reference Sheet R5: **which columns a client may filter on, and with which operators**
        // (contracts §3.2, decision 0062). Empty when the schema declares nothing filterable.
        //
        // Per column rather than a flat operator list, because the operators are a property of the
        // column's family: a category takes `eq`/`in` over its value set, a `keyword` column takes
        // the four string predicates, and a `text` column takes `match` and nothing else. A client
        // that had to infer this from `arrow_type` would be re-deriving the schema's own rule, and
        // would get `text` wrong — its type is a string type and its operand is not a string
        // predicate.
        //
        // **`family` is what tells a viewer which control to draw.** A category has a value set, so
        // `/v1/categories/{column}` fills a dropdown. A string has none — its values are row data,
        // not a vocabulary, so nothing enumerates them and the control is a free-text box. That is a
        // data-model fact rather than a missing endpoint (per-point-attributes; `filter-index.md`
        // §2.3), and a client that expected a value list for a string would be waiting for an
        // endpoint that will never exist.
        //
        // `keyword` is published as its own family beside `string`, and takes the same four
        // operators, because a client draws the same control for both and the difference between
        // them is a storage one. Naming it is still worth a word on the wire: a keyword's values are
        // held in a sorted dictionary the server never serves — no listing, no autocomplete, no
        // `/v1/categories` counterpart (records §4.3) — so a client that reads `keyword` as
        // *enumerable* would be waiting for the same endpoint that will never exist.
        //
        // The combinators (`all_of`, `any_of`, `none_of`) are not published per column — they
        // compose expressions rather than belonging to one. ⊘ **`none_of` is not universal over the
        // columns published here, and nothing on this surface says so**: it subtracts from the set
        // of items *carrying a value*, and a `text` column stores no per-item value to be present,
        // so the engine refuses a negation over one (`FilterError::NegationWithoutPresence`). A
        // client discovers that from the refusal rather than from the operand list, which is the
        // wrong way round; publishing negatability per column is what would fix it.
        //
        // **The predicate is the engine's** (`filter::is_filterable`, decision 0068): `index`
        // columns, plus every rendered one — the render-only ones answered over the request's own
        // rows. The viewport parse gates on the same function, so the surface a client is
        // published here cannot differ from the one its requests are held to.
        //
        // A rendered **number** is on this list with its family's full operator set, `range`
        // included: the hot column cannot express absence, so decision 0064 puts it in a presence
        // bitmap beside the column that the row scan reads. A client cannot tell which route
        // answered — that is 0068's whole licence to have two.
        "filter_operands": meta.declared_scalars.iter().filter(|d| tessera_engine::filter::is_filterable(d)).map(|d| {
            let family = family_of(d);
            serde_json::json!({
                "column": d.name,
                "family": family.as_str(),
                "operands": family.operands(),
            })
        }).collect::<Vec<_>>(),
        // §7.2's selection constants. A client cannot read mark count as density without knowing
        // where the floor and the cap sit, so these are a genuine client need rather than test
        // convenience -- and the reference oracle cannot reproduce the definition without them.
        //
        // They disclose nothing. All of them are deployment constants, identical for every
        // principal. Publishing `theta_target_marks` lets a client solve for theta's anchor, which
        // is the composed cardinality of its OWN mask over the whole view -- precisely what a
        // `zoom = 0`, full-bbox request already returns as `visible` in a single call (§7.1).
        // Already obtainable, exactly.
        //
        // `max_k` is published for the same reason. Contracts §3.2 tells the client the effective
        // cap is `min(k, max_k, k_max_marks)`, so publishing only one of the two ceilings would
        // leave a client unable to learn its own request bound, and an independent implementation
        // unable to tell a **cap-clause** refusal from a **machine-ceiling** one. That distinction
        // is exactly the one §7.2 insists on keeping -- the overplot ceiling and the machine
        // ceiling are deliberately not the same knob, because raising the machine ceiling on
        // transport evidence must not silently dissolve §7.2's cap clause. A client that cannot see
        // both cannot honour it either.
        "selection": {
            "k_min": selection.k_min,
            "k_max_marks": selection.k_max_marks,
            "max_k": state.max_k,
            "theta_target_marks": selection.theta_target_marks,
            "max_underlay_offset": selection.max_underlay_offset,
            // Published for exactly the reason `max_k` is: a client
            // that chooses its own request *depth* — the mark-budget work — is choosing a tile
            // count, and without this it cannot tell whether a refusal was its own arithmetic or
            // the deployment's ceiling. It discloses nothing: a deployment constant, identical for
            // every principal, and the tile grid is public.
            "max_tiles_per_request": selection.max_tiles_per_request,
            // `/v1/categories`' page ceiling, published for the same reason the others are: a
            // client that pages must know when a short page means "the set ended" rather than
            // "the deployment truncated".
            "max_category_values": state.max_category_values,
        },
        // The annotation layers this principal may know exist, and what each declared.
        //
        // **Never the artifact cardinality.** A count of artifacts in a layer is a corpus-wide
        // count over objects the principal may not individually see, which is C8's row — and it is
        // the obvious field to add here, which is why its absence is stated rather than left to be
        // noticed. Nothing on this document says how big a layer is.
        //
        // What *is* published is the declaration: identity, structure, the derived vocabulary a
        // client must know to draw anything, which views the layer appears in, and what kinds of
        // supplied content its artifacts carry. Publishing the supplied *kinds* is safe because an
        // artifact failing containment is absent whole, so no served artifact ever lacks a content
        // its layer declared — there is no shell to be distinguishable from absence.
        //
        // The gate label is **not** published. A caller who reaches a layer has already satisfied
        // it, so the label adds nothing they can act on; a caller who has not never sees the entry.
        // Publishing it would put a term name on a document whose whole purpose is that the
        // unreachable case is indistinguishable from the nonexistent one.
        "layers": layers.iter().map(|layer| {
            let d = &layer.declaration;
            serde_json::json!({
                "name": d.name,
                "title": d.title,
                "views": d.views,
                "membership": d.membership,
                "hierarchy": {
                    "kind": d.hierarchy.kind,
                    // The layer's **default** cut depth, not its only setting: a viewport request
                    // may ask for more detail than it (decision 0083). A budget is not a disclosure
                    // control — every artifact a deeper cut returns has passed its own existence
                    // test — which is why the default is publishable and adjustable at all.
                    "prune_children": d.hierarchy.prune_children,
                },
                // Empty for a treed layer, which declares none: its lineage is in its edges, and a
                // level number would say nothing about position in it (decision 0082).
                "levels": d.levels.iter().map(|l| serde_json::json!({
                    "level": l.level,
                    "title": l.title,
                    "zoom": l.zoom.map(|(lo, hi)| serde_json::json!([lo, hi])),
                })).collect::<Vec<_>>(),
                "computed_content": d.content.computed,
                // The **types**, as before: a client draws from them, and publishing them is safe
                // because an artifact failing containment is absent whole. ⊘ Each entry's `name`
                // — which distinguishes two contents of one type on one layer — is declared and
                // not yet published; the wire shape is contracts', not this stage's, to widen.
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
    /// Comma-separated codes to resolve. Present means bulk lookup; absent means enumerate.
    #[serde(default)]
    codes: Option<String>,
    /// Resume enumeration after this value **key** — the cursor.
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /v1/categories/{column}` (contracts §3.2): what this column's codes stand for.
///
/// **Two forms, one gate.** `?codes=` resolves the codes a caller already holds — the viewer's
/// normal path, since it knows exactly which codes it drew — and the bare form pages the whole
/// value set. Both run `Engine::categories`, which applies `visibility` before the forms diverge; a
/// gate reached by one door and not the other is the existence oracle by another route.
///
/// **404 `unknown` covers three cases and distinguishes none of them**: no such column, a column
/// that is a plain scalar rather than a category, and a column whose vocabulary is missing. What a
/// caller may learn about which columns exist is `/v1/meta`'s answer, and this route must not
/// become a second, finer one.
///
/// **Not behind the compute gate**, unlike `/v1/viewport` and `/v1/items`. The work is a bounded
/// walk of an in-memory `BTreeMap` — no mask composition, no projection, no file IO — so it is the
/// same class of request as `/v1/meta`, which is also ungated. There is nothing here for a queue
/// to protect.
async fn categories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(column): AxumPath<String>,
    AxumQuery(query): AxumQuery<CategoriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    // Clamped, not refused: the ceiling is a response bound rather than a disclosure control, so a
    // caller asking for more than the deployment serves gets the deployment's answer plus a cursor
    // — which is what pagination is for. `0` is refused, because a zero-length page with a cursor
    // that never advances is an infinite loop dressed as a response.
    let limit = match query.limit {
        Some(0) => {
            return Err(ApiError::Contract(
                "limit must be at least 1; a zero-length page cannot make progress".to_string(),
            ))
        }
        Some(n) => n.min(state.max_category_values),
        None => state.max_category_values,
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

    let query = match &codes {
        Some(codes) => tessera_engine::CategoryQuery::Codes(codes),
        None => tessera_engine::CategoryQuery::Page {
            after: query.after.as_deref(),
            limit,
        },
    };

    let page = state
        .engine
        .categories(&entry.session, &column, query)
        .map_err(map_engine_error)?
        .ok_or_else(|| ApiError::Unknown("unknown category column".to_string()))?;

    Ok(Json(serde_json::json!({
        "column": page.column,
        "values": page.values.iter().map(|v| serde_json::json!({
            "code": v.code,
            "key": v.key,
            // Omitted rather than null when no author wrote one — which is every value a
            // discovered vocabulary mints. The key is the display fallback (§3.1).
            "title": v.title,
        })).collect::<Vec<_>>(),
        "next": page.next,
    })))
}

/// A filterable column's family — **one derivation, used by `/v1/meta`, the parser and the engine's
/// own routing alike**, so the operator list a client is published cannot differ from the one it is
/// held to, nor from the rules the scan reads its values by. It lives in the engine because the row
/// route needs it too: a rendered `u8` category and a rendered `u8` number are the same bytes and
/// have opposite absence rules.
fn family_of(d: &tessera_engine::DeclaredScalar) -> tessera_engine::filter::Family {
    tessera_engine::filter::Family::of(d)
}

#[derive(Debug, Deserialize)]
struct ViewportReq {
    view: String,
    zoom: u8,
    /// Absent exactly when `tiles` is present — the two are alternatives, not a pair.
    #[serde(default)]
    bbox: Option<[f64; 4]>,
    /// The exact depth-`zoom` Morton prefixes to answer for, in place of a bbox.
    ///
    /// A client holding a replica omits every tile it can prove it already has, and an omitted tile
    /// costs the engine nothing at all — no range derivation, counting, selection or gather. That
    /// is what makes server work scale with novelty rather than with viewport area.
    ///
    /// Refused, never silently preferred, when a bbox is sent too: two ways of naming a tile set in
    /// one request is a contradiction the server must not resolve on the caller's behalf, and
    /// `/v1/region` already sets that precedent with its "exactly one of polygon/bbox".
    #[serde(default)]
    tiles: Option<Vec<u64>>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    pin: Option<PinDto>,
    /// §3.3 density underlay: serve exact masked counts at depth `zoom + underlay_offset`. Absent
    /// or `0` means no underlay and no extra bytes.
    ///
    /// Rejected, never clamped, on all three bounds (configured maximum, the depth-16 grid limit,
    /// and the total sub-cell budget) — a Morton prefix carries no depth of its own, so a silently
    /// reduced offset would hand back cells the client could not interpret.
    #[serde(default)]
    underlay_offset: Option<u8>,
    /// The filter expression (contracts §3.2), parsed by [`crate::filter_dto`].
    ///
    /// Absent is the unfiltered request. A malformed expression, an unknown column or an unbuilt
    /// operator is a `422`; an unknown **value** is not — see that module's header for why the two
    /// must not be conflated.
    #[serde(default)]
    filters: Option<serde_json::Value>,
    /// Which annotation layers to answer for — and, with them, which membership columns the
    /// points frames carry (D12).
    ///
    /// **Absent, or the empty list, answers for none and costs nothing; the string `"all"`
    /// answers for every layer this principal reaches** (owner ruling 2026-08-25, contracts
    /// §3.2). A client that never thinks about layers therefore never pays the artifact pass,
    /// and one that wants everything says so. `all` is reserved — a layer cannot be registered
    /// under it — so the word is never ambiguous.
    ///
    /// **A list narrows and never widens.** A name this principal does not reach is absent from
    /// the answer whether or not it was asked for, by the same route a name nobody registered is —
    /// so naming a layer is not a way to learn whether it exists.
    #[serde(default)]
    layers: Option<LayersReq>,
    /// How many artifacts the client wants back at most, in the same shape as `k` beside it.
    ///
    /// **Honoured structurally, never by sampling** ([decision 0083](../../../docs/decisions/0083-the-frontier-is-a-request-time-budget.md)):
    /// a budget that cannot be met by serving everything is met by serving ancestors instead of
    /// their descendants. A flat layer has no ancestors, so today this is accepted and inert — the
    /// field is defined now because it is a wire shape, and adding a request field to a shipped
    /// frame later is the change this ordering exists to avoid.
    #[serde(default)]
    artifact_budget: Option<u32>,
    /// Which of each named layer's declared levels to answer for.
    ///
    /// **Absent follows the layer's own declaration** — the levels whose declared zoom range
    /// contains this request's `zoom`, which is what a client reading `/v1/meta`'s zoom→level map
    /// would have asked for and until now had no way to say. The string `"all"` answers for every
    /// level; a list answers for exactly those, and **an empty list is *none*, as `layers: []` is**.
    ///
    /// **A layer that declares no levels at all is inert to this in every form** — a treed or flat
    /// layer sits entirely at level 0, so a request naming levels for the tiered layer beside it
    /// does not blank its clusterings. **A layer that declares levels but no zoom range on any of
    /// them serves every level in the absent case**, there being no map to follow.
    ///
    /// **It applies to every layer named.** A level number is a rung of one layer and means nothing
    /// across two, so there is no per-layer map here; under decision 0096 a request names one layer
    /// anyway, and the absent case needs no map at all because each layer's own ranges decide for
    /// it.
    ///
    /// **A level a layer does not hold is absent, not a refusal** — the same route an unreachable
    /// layer name takes, and the same reason: asking is not a way to learn what exists.
    #[serde(default)]
    levels: Option<LevelsReq>,
}

/// The `layers` field's two spellings: a list of names, or the one reserved word.
///
/// Untagged, so the JSON is `["a", "b"]` or `"all"` and nothing else: any other string is a
/// `422` from serde rather than a name that silently matches no layer, which is what an
/// `Option<Vec<String>>` accepting a stray string would have had to become.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LayersReq {
    All(AllLayers),
    Named(Vec<String>),
}

/// The `levels` field's two spellings: a list of level numbers, or the one reserved word.
///
/// Untagged on the same argument as [`LayersReq`]: `[0, 1]` or `"all"`, and any other string is a
/// `422` rather than a selection that silently matches nothing.
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

/// The literal `"all"` and only that — `tessera_types::layer::RESERVED_LAYER_SELECTION`.
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

/// The streamed viewport's channel capacity, in frames. Two: one in flight to hyper, one built
/// ahead — the backpressure bound `streamed-serving.md` §5 states, and all a stalled client can
/// hold beyond hyper's own write buffer once the producer has been shed.
const STREAM_CHANNEL_FRAMES: usize = 2;

/// [`StreamBody`]'s three completion states, published by the producer *before* it drops the
/// channel sender, so the body's end-of-channel read is never ambiguous.
const STREAM_RUNNING: u8 = 0;
const STREAM_COMPLETE: u8 = 1;
const STREAM_ABORTED: u8 = 2;

/// One streamed `/v1/viewport` producer→handler handoff: everything the handler needs to
/// construct the `Response` — the header coordinates and the serialised first flush — sent once,
/// when the engine's sweep completes. Errors up to that point travel the same oneshot as `Err`,
/// so every pre-first-flush failure keeps its typed status exactly as before streaming
/// (`streamed-serving.md` §5).
struct FirstFlush {
    coordinates: tessera_engine::ViewCoordinates,
    stamp: GenerationStamp,
    stale: bool,
    /// The serialised tiles frame plus, when the §3.3 underlay was requested, the sub-cells
    /// frame — the body's first bytes, prepended ahead of the channel.
    first_frames: Vec<u8>,
    /// Post-admission time to first-flush-ready, µs — the `x-tessera-server-us` header, which is
    /// the latency gate's figure; the whole-stream total is the trailer's `stream_us`.
    server_us: u64,
}

/// The engine's [`ViewportSink`], wired to the transport: serialises each delivery to frames
/// (`tessera-wire`), hands the first flush to the waiting handler, and blocking-sends the rest
/// into the bounded body channel under the two deadlines. Owns the request's [`GatePermits`] so
/// the compute permit can be released at exactly the sweep/emit boundary and the slot permit
/// exactly when the producer returns.
///
/// I10 is upheld structurally on this path exactly as before streaming: no entity id is
/// available to leak here, because the engine never gathers one (`tessera_engine::PointColumns`'
/// doc) — the wire identity is `tessera_id` carried through unchanged, and scalar names come
/// from the head, minted from the SAME generation the points are gathered from (never a second
/// `Engine::meta` call — lifecycle §1.1).
struct WireSink {
    head: Option<ViewportHead>,
    first_tx: Option<oneshot::Sender<Result<FirstFlush, ApiError>>>,
    tx: mpsc::Sender<Bytes>,
    permits: GatePermits,
    /// Taken post-admission, before the producer was spawned, so blocking-pool scheduling wait
    /// lands inside `server_us` — the same accounting as before streaming.
    start: Instant,
    stall: Duration,
    deadline: Duration,
    /// Set at the first flush; the whole-stream deadline is measured from it.
    first_flush_at: Option<Instant>,
    /// **Why this sink stopped accepting frames**, where it stopped for a reason of its own.
    ///
    /// A refusal reaches the engine as `SinkClosed` and comes back as `EngineError::Cancelled`,
    /// which the mid-body arm treats as *the client went away* and deliberately does not log. That
    /// is right for a disconnect and wrong for a shed: the 2026-08-22 campaign found a first
    /// request truncated at 111 s with **neither** `viewport stream aborted` line firing, because
    /// the server's own deadline had fired and had no way to say so. This is that way.
    shed: Option<Shed>,
    arrow_serialise_ns: u64,
    points_total: u64,
    flushes: u64,
}

/// A sink refusal the **server** chose, told apart from the client going away.
#[derive(Debug, Clone, Copy)]
enum Shed {
    /// The whole-stream budget from first flush, `serve.stream_deadline_ms`.
    Deadline,
    /// The per-send stall budget, `serve.stream_write_stall_ms` — a reader that stopped reading.
    Stall,
}

impl Shed {
    fn detail(self) -> &'static str {
        match self {
            Shed::Deadline => "the whole-stream deadline fired: the response was committed and the                                work behind its next frame outran serve.stream_deadline_ms. A cold                                request over a level whose derived structures the prefix does not                                carry is the shape to check first — the build's artifact pass                                writes them, and an open reporting no adoptions says they were not                                taken",
            Shed::Stall => "the per-send stall budget fired: the client stopped reading and                             serve.stream_write_stall_ms elapsed with the body channel full",
        }
    }
}

impl WireSink {
    /// Blocking-send one frame under the two deadlines `streamed-serving.md` §5 requires: a
    /// per-send stall budget (a reader that stopped) and a whole-stream budget from first flush
    /// (a reader that drips — the shape a per-send deadline alone admits). Refusal is
    /// [`SinkClosed`], which the engine treats as cancellation.
    fn send(&mut self, frame: Vec<u8>) -> SinkResult {
        let send_started = Instant::now();
        let mut item = Bytes::from(frame);
        loop {
            if self
                .first_flush_at
                .is_some_and(|t| t.elapsed() >= self.deadline)
            {
                self.shed = Some(Shed::Deadline);
                return Err(SinkClosed);
            }
            match self.tx.try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(back)) => {
                    if send_started.elapsed() >= self.stall {
                        self.shed = Some(Shed::Stall);
                        return Err(SinkClosed);
                    }
                    item = back;
                    // A sleep poll, 5 ms against a 10 s default stall budget: tokio's mpsc has
                    // no blocking-send-with-timeout, and waking a sync thread from the async
                    // receiver would need a second channel to save a wait this coarse. The
                    // parked thread is the one this request already holds.
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(SinkClosed),
            }
        }
    }
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
        let mut frames = tiles_frame(&tile, &visible, &matched, &served);
        // `Some` of an empty slice is a present, zero-row frame; `None` is no frame at all —
        // presence is decided by the request, not the result (contracts §3.2's r12 rule).
        if let Some(cells) = sub_cells {
            let cell: Vec<u64> = cells.iter().map(|c| c.cell).collect();
            let count: Vec<u64> = cells.iter().map(|c| c.count).collect();
            frames.extend_from_slice(&sub_cells_frame(&cell, &count));
        }
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;

        // The sweep is done: the compute permit goes back to the gate here, at exactly the
        // boundary where this request stops computing at its own pace and starts emitting at
        // the client's (`streamed-serving.md` §5). The slot permit stays until the producer
        // returns.
        self.permits.release_compute();
        self.first_flush_at = Some(Instant::now());

        let head = self.head.as_ref().expect("head precedes counts");
        let first = FirstFlush {
            coordinates: head.coordinates,
            stamp: head.stamp.clone(),
            stale: head.stale,
            first_frames: frames,
            server_us: self.start.elapsed().as_micros() as u64,
        };
        // A dropped receiver is the handler future gone — the client disconnected during the
        // sweep — which is the same signal as a closed body channel: stop.
        self.first_tx
            .take()
            .expect("counts is delivered exactly once")
            .send(Ok(first))
            .map_err(|_| SinkClosed)
    }

    /// Never called with an empty slice — the engine skips it, on the points frame's rule.
    ///
    /// Sent as an ordinary body frame rather than folded into the first flush: the first flush is
    /// already gone by the time this runs (the counts callback sends it), and moving the artifact
    /// sweep ahead of the counts would delay the number channel behind it — which
    /// `streamed-serving.md` §2 puts first deliberately.
    fn artifacts(&mut self, artifacts: &[tessera_engine::ArtifactOut]) -> SinkResult {
        let serialise_start = Instant::now();
        let rows: Vec<ArtifactRow<'_>> = artifacts
            .iter()
            .map(|a| ArtifactRow {
                layer: a.layer.as_str(),
                tessera_id: a.tessera_id.raw(),
                key: a.key.as_deref(),
                masked_count: a.masked_count,
                centroid: a.derived.centroid,
                bbox: a.derived.bbox,
                hull: a.derived.hull.as_deref(),
                content: &a.content,
                parent_id: a.parent_id.map(|id| id.raw()),
                level: a.level,
            })
            .collect();
        let frame = artifacts_frame(&rows);
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;
        self.send(frame)
    }

    fn points(&mut self, chunk: tessera_engine::PointColumns) -> SinkResult {
        let head = self.head.as_ref().expect("head precedes points");
        let serialise_start = Instant::now();
        // The wire's buffers are the engine's buffers borrowed — no transpose, no reshaping;
        // names zip positionally with the chunk's columns. Both lists are the **render**
        // narrowing of the declaration, in declaration order: the compiled schema is wider
        // (filter-only and blob-resident columns occupy no points buffer), and the head carries
        // the narrowed list precisely so this zip cannot pair a buffer with a wider list's name.
        let scalar_refs: Vec<(&str, ScalarColumn)> = head
            .render_scalars
            .iter()
            .zip(&chunk.scalars)
            .map(|(d, col)| (d.name.as_str(), column_ref(col)))
            .collect();
        // The membership columns (D12) name themselves: which layers get one is settled by the
        // artifact pass, after the head, so the chunk carries the names rather than the head.
        let membership_refs: Vec<(&str, &[Option<u64>])> = chunk
            .membership
            .iter()
            .map(|m| (m.layer.as_str(), m.ids.as_slice()))
            .collect();
        let frame = points_frame(
            &chunk.tessera_ids,
            &chunk.codes,
            &scalar_refs,
            &membership_refs,
        );
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;
        self.points_total += chunk.tessera_ids.len() as u64;
        self.flushes += 1;
        self.send(frame)
    }
}

/// The producer: the engine call through frame serialisation, run inside `spawn_blocking` for
/// the whole life of the stream. Returns nothing — every outcome is communicated through the
/// oneshot (pre-first-flush errors), the channel (frames), or the shared state (completion
/// versus abort), and the permits release when `sink` drops at this function's end.
fn run_viewport_stream(
    state: &AppState,
    session: &tessera_engine::Session,
    req: ViewportReq,
    cancel: CancelToken,
    mut sink: WireSink,
    shared: Arc<AtomicU8>,
) {
    let stamp = req.pin.map(GenerationStamp::from);
    // Contracts §3.2's default. It is the deployment's own overplot ceiling rather than a
    // literal, so a client that expresses no preference gets the full budget this deployment will
    // serve and §7.2's proportional window is realised in full — at the old default of 30 against a
    // cap of 128 the window was 15 rather than 64, i.e. the default silently threw away most of the
    // density range the parameters were chosen for.
    let k = req
        .k
        .unwrap_or_else(|| state.engine.config().k_max_marks)
        .min(state.max_k);

    // **Deduplicated here, not trusted from the caller — first occurrence kept, order
    // preserved.** A repeated tile would be served — and drawn — twice, inflating every count a
    // client derives from the response. The order is NOT normalised: the request's own order is
    // the response's order (contracts §3.2), which is how a streaming client gets its tiles
    // centre-out by ordering its own list. The engine's range derivation sorts internally, so
    // arbitrary order costs nothing there.
    let tiles = req.tiles.map(|list| {
        let mut seen = std::collections::HashSet::with_capacity(list.len());
        list.into_iter()
            .filter(|t| seen.insert(*t))
            .collect::<Vec<_>>()
    });
    // Validated as present-and-alone by the handler before admission; the default here is inert.
    let bbox = req.bbox.unwrap_or([0.0, 0.0, 0.0, 0.0]);

    // **Parsed against the live schema, before any compute.** A malformed expression must not reach
    // the engine, and a `422` here costs a request nothing — where refusing after the mask is built
    // has already paid for a fragment. It is also before the first frame is written, which matters
    // more under streaming than it did before it: once a frame is out the status line is spent, and
    // a filter refused mid-stream could only be reported as a truncation.
    let filter = match &req.filters {
        None => None,
        Some(value) => {
            let meta = state.engine.meta();
            // The same predicate `/v1/meta`'s operand list publishes — the engine's
            // `filter::is_filterable` — so a column a client was told about parses and a column
            // it was not stays the unknown-column 422.
            let filterable: std::collections::HashMap<&str, tessera_engine::filter::Family> = meta
                .declared_scalars
                .iter()
                .filter(|d| tessera_engine::filter::is_filterable(d))
                .map(|d| (d.name.as_str(), family_of(d)))
                .collect();
            let vocab_of: std::collections::HashMap<&str, &str> = meta
                .declared_scalars
                .iter()
                .filter_map(|d| Some((d.name.as_str(), d.vocabulary.as_deref()?)))
                .collect();
            match crate::filter_dto::parse(
                value,
                &|column| filterable.get(column).copied(),
                &|column, key| {
                    let vocabulary = vocab_of.get(column)?;
                    meta.vocabularies.get(vocabulary)?.code_of(key)
                },
            ) {
                Ok(expr) => Some(expr),
                // The pre-first-flush channel, the same one an engine refusal takes: nothing is
                // committed, the handler is still waiting on it, and the typed `422` reaches the
                // client exactly as it did before streaming.
                Err(e) => {
                    if let Some(tx) = sink.first_tx.take() {
                        let _ = tx.send(Err(e));
                    }
                    return;
                }
            }
        }
    };

    // **Owned copies of what names the request**, taken before the engine borrows `req`, so the
    // shed log below can say which request it was without extending a borrow across the call.
    // Three coordinates and no principal: a view id, a zoom and the layer names the caller asked
    // for, all of them the caller's own words back.
    let named_view = req.view.clone();
    let named_zoom = req.zoom;
    let named_layers = match &req.layers {
        Some(LayersReq::All(_)) => tessera_types::layer::RESERVED_LAYER_SELECTION.to_string(),
        Some(LayersReq::Named(names)) => names.join(","),
        None => String::new(),
    };

    // **Omitted is the empty list**, and the mapping is the one place the wire's default is
    // decided. Borrowed as `&[&str]` for the engine's request, which holds the list rather than
    // owning it.
    let layer_names: Vec<&str> = match &req.layers {
        Some(LayersReq::Named(names)) => names.iter().map(String::as_str).collect(),
        Some(LayersReq::All(_)) | None => Vec::new(),
    };
    let layers = match &req.layers {
        Some(LayersReq::All(_)) => LayerSelection::All,
        Some(LayersReq::Named(_)) | None => LayerSelection::Named(&layer_names),
    };
    // **Omitted is the declaration's own map**, which is the opposite default from `layers` beside
    // it and deliberately so: naming a layer has already opted into the artifact pass, and what is
    // left is which of its rungs to answer at. The expensive answer is *every level*, so that is
    // the one a caller asks for by name.
    let level_numbers: Vec<u32> = match &req.levels {
        Some(LevelsReq::Named(levels)) => levels.clone(),
        Some(LevelsReq::All(_)) | None => Vec::new(),
    };
    let levels = match &req.levels {
        Some(LevelsReq::All(_)) => LevelSelection::All,
        Some(LevelsReq::Named(_)) => LevelSelection::Named(&level_numbers),
        None => LevelSelection::Declared,
    };
    let mut request = ViewportRequest::new(&req.view, req.zoom, bbox, k)
        .tiles(tiles.as_deref())
        .stamp(stamp)
        .underlay_offset(req.underlay_offset)
        .layers(layers)
        .artifact_budget(req.artifact_budget)
        .levels(levels)
        .cancel(Some(cancel));
    if let Some(filter) = filter {
        request = request.filter(filter);
    }

    let outcome =
        state
            .engine
            .viewport_stream(session, request, state.stream_flush_bytes, &mut sink);

    match outcome {
        Ok(timings) => {
            // The trailer: exactly this key set, and the conformance comparator asserts it —
            // the one server-authored JSON region of the body must not quietly acquire a field
            // the comparator never sees (`streamed-serving.md` §7). `stream_us` includes
            // client-paced channel waits and is deliberately NOT named `server_us`: the
            // server-cost figure is the first-flush header.
            let mut trailer = serde_json::json!({
                "stream_us": sink.start.elapsed().as_micros() as u64,
                "arrow_serialise_ns": sink.arrow_serialise_ns,
                "points": sink.points_total,
                "flushes": sink.flushes,
            });
            if state.stage_timing {
                if let Some(csv) = stage_header(&timings, sink.arrow_serialise_ns) {
                    trailer["stage_ns"] = serde_json::Value::String(csv);
                }
            }
            let frame = trailer_frame(trailer.to_string().as_bytes());
            let end_state = if sink.send(frame).is_ok() {
                STREAM_COMPLETE
            } else {
                STREAM_ABORTED
            };
            shared.store(end_state, Ordering::SeqCst);
        }
        Err(e) => match sink.first_tx.take() {
            // Pre-first-flush: nothing is committed, and the handler is waiting on this
            // channel — the typed status reaches the client exactly as before streaming,
            // including `Cancelled → 500 fail-closed` (a pre-status cancellation means the
            // requester is gone and nobody reads the status).
            Some(tx) => {
                let _ = tx.send(Err(map_engine_error(e)));
            }
            // Mid-body: the 200 is committed. No trailer is ever sent — the missing kind-4
            // frame marks the response incomplete — and the body wrapper turns the channel's
            // end into a transport abort, so the truncation is loud twice over
            // (`streamed-serving.md` §6). Cancellation here is the client's own disconnect or
            // shed and logs nothing; anything else is a server fault worth a line.
            None => {
                // **A shed the server chose is not a client disconnect**, and until this branch
                // existed the two were the same silence — see [`WireSink::shed`]. Named loudly and
                // with the elapsed figure, because the elapsed figure is the diagnosis: a whole
                // number of seconds past the deadline is a client that stopped reading, and a
                // multiple of it is work behind the next frame.
                if let Some(shed) = sink.shed {
                    tracing::warn!(
                        view = %named_view,
                        zoom = named_zoom,
                        layers = %named_layers,
                        elapsed_ms = sink.start.elapsed().as_millis() as u64,
                        since_first_flush_ms = sink
                            .first_flush_at
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0),
                        deadline_ms = sink.deadline.as_millis() as u64,
                        stall_ms = sink.stall.as_millis() as u64,
                        flushes = sink.flushes,
                        "viewport stream SHED mid-body by the server — {}",
                        shed.detail()
                    );
                } else if !matches!(e, tessera_engine::EngineError::Cancelled) {
                    tracing::warn!(error = %e, "viewport stream aborted mid-body");
                }
                shared.store(STREAM_ABORTED, Ordering::SeqCst);
            }
        },
    }
    // `sink` drops here — after the state stores above, which is what makes the body wrapper's
    // end-of-channel read unambiguous — releasing the channel sender and, through
    // `GatePermits`' drop, the slot permit (and the `streaming` gauge entry, if the sweep
    // completed).
}

/// The streamed response body: the first flush, then the producer's frames, then — only when
/// the shared state says the trailer went out — a clean end. Owns the request's [`CancelGuard`]
/// for the response's lifetime: a client that disconnects mid-stream drops this body, which
/// flips the token (the producer observes it at its next checkpoint) and closes the channel
/// (the producer's next send fails immediately).
struct StreamBody {
    first: Option<Bytes>,
    rx: mpsc::Receiver<Bytes>,
    shared: Arc<AtomicU8>,
    cancel_guard: CancelGuard,
    /// Fused: once the end (clean or aborted) has been yielded, every later poll is
    /// `Ready(None)` rather than a second abort error — hyper stops at the first, but a
    /// combinator that polls past it must not observe a stream that un-ends.
    done: bool,
}

impl futures_core::Stream for StreamBody {
    type Item = std::result::Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if let Some(first) = this.first.take() {
            return Poll::Ready(Some(Ok(first)));
        }
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(None) => {
                this.done = true;
                if this.shared.load(Ordering::SeqCst) == STREAM_COMPLETE {
                    // Clean end: the trailer was the last frame, the producer has returned,
                    // and disarming keeps "cancelled" meaning "cut short" — see `CancelGuard`.
                    this.cancel_guard.disarm();
                    Poll::Ready(None)
                } else {
                    // Aborted — an emit-phase engine error, a stall shed, or the stream
                    // deadline. Surfacing an error makes hyper cut the connection rather than
                    // end the chunked body cleanly: the missing trailer already marks the
                    // response incomplete, and this makes it loud (`streamed-serving.md` §6).
                    Poll::Ready(Some(Err(std::io::Error::other(
                        "viewport stream aborted before its trailer",
                    ))))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

async fn viewport(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ViewportReq>,
) -> Result<Response, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    if req.zoom > 16 {
        return Err(ApiError::Contract("zoom must be in 0..=16".to_string()));
    }

    // **Exactly one of `bbox` and `tiles`.** Both is a contradiction the server must not resolve on
    // the caller's behalf — silently preferring one would leave a client believing it had asked for
    // a region it never received — and neither leaves nothing to answer for. `/v1/region`'s
    // "exactly one of polygon/bbox" is the same shape.
    match (&req.bbox, &req.tiles) {
        (Some(bbox), None) => {
            if bbox.iter().any(|v| !v.is_finite()) || bbox[0] > bbox[2] || bbox[1] > bbox[3] {
                return Err(ApiError::Contract(
                    "bbox must be [x0, y0, x1, y1] with x0 <= x1, y0 <= y1, all finite".to_string(),
                ));
            }
        }
        (None, Some(tiles)) => {
            // A prefix carries no depth of its own, so one with bits above `zoom` names a tile at a
            // depth this request is not asking about. Refused rather than masked off, for the same
            // reason `underlay_offset` is: a silently reduced request hands back tiles the client
            // cannot interpret.
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

    // The cancellation token and its drop-guard, created before the admission-gate acquire below
    // so the guard's lifetime spans the whole handler — a disconnect during the queue
    // wait is already free (dropping the `admit().await` future releases nothing that was ever
    // acquired), but creating the guard here rather than after admission keeps one token identity
    // for the entire request and costs nothing extra. It moves into the `StreamBody` below, so
    // its reach extends to the whole response, not just this handler's await.
    let cancel = CancelToken::new();
    let cancel_guard = CancelGuard::new(cancel.clone());

    // The two-stage admission gate. `admit()` sheds with `ApiError::Backpressure` (429) if the
    // outer slots semaphore has no permit to `try_acquire`, or if the inner compute semaphore does
    // not free one within `admission_timeout_ms`. `admission_us` is the queue wait, reported below
    // as `x-tessera-admission-us`.
    let (gate_permits, admission_us) = state.compute_gate.admit().await?;

    // Server-side timing, for the latency gate `scripts/bench_p99.py` checks. Not a wire-format
    // field — an observability-only response header, reported to microseconds so the gate can be
    // checked without relying on end-to-end (client-observed) latency.
    //
    // **The clock starts AFTER admission and stops at first-flush-ready**, so
    // `x-tessera-server-us` means "server compute to the first drawable flush, excluding
    // queueing" — the server-cost part of a streamed response, and the latency gate's figure;
    // the whole-stream wall total (which includes client-paced sends) is the trailer's
    // `stream_us`, deliberately under a different name. `start` is taken before
    // `spawn_blocking`, so a blocking-pool scheduling wait lands inside `server_us` rather than
    // in `x-tessera-admission-us`, exactly as before streaming.
    let start = Instant::now();

    // The producer: engine call through frame serialisation, CPU-bound then client-paced, with
    // no `.await` of its own — `spawn_blocking` moves it to tokio's blocking-thread pool, where
    // it lives for the whole stream (parked in channel sends while the client reads;
    // `streamed-serving.md` §5 states the pool arithmetic).
    //
    // **Detached, deliberately.** The handler awaits the first flush on the oneshot below, not
    // the closure itself: the closure outlives this handler by the length of the stream. Its
    // panic is observable as the oneshot closing (mapped to the same fail-closed 500 the old
    // `map_join_error` produced), or — after the first flush — as a body abort.
    //
    // Closure capture: `state` is a cloned `Arc<AppState>`, `entry` the `Arc<SessionEntry>`,
    // `req` moved whole, `sink` carries the `GatePermits` (released at the sweep/emit boundary
    // and at producer exit — see `WireSink`), and only a *clone* of `cancel` moves in:
    // `cancel_guard` keeps the original, first here, then inside the response body.
    let (first_tx, first_rx) = oneshot::channel();
    let (tx, rx) = mpsc::channel::<Bytes>(STREAM_CHANNEL_FRAMES);
    let shared = Arc::new(AtomicU8::new(STREAM_RUNNING));
    let sink = WireSink {
        head: None,
        first_tx: Some(first_tx),
        tx,
        permits: gate_permits,
        start,
        stall: Duration::from_millis(state.stream_write_stall_ms),
        deadline: Duration::from_millis(state.stream_deadline_ms),
        first_flush_at: None,
        shed: None,
        arrow_serialise_ns: 0,
        points_total: 0,
        flushes: 0,
    };
    let closure_state = Arc::clone(&state);
    let closure_cancel = cancel.clone();
    let closure_shared = Arc::clone(&shared);
    drop(tokio::task::spawn_blocking(move || {
        run_viewport_stream(
            &closure_state,
            &entry.session,
            req,
            closure_cancel,
            sink,
            closure_shared,
        );
    }));

    let first = match first_rx.await {
        Ok(Ok(first)) => first,
        // Every pre-first-flush failure, with its typed status — auth ran earlier, so these are
        // the engine's own refusals (422/404/429/500), exactly the set the awaited-closure
        // shape produced.
        Ok(Err(e)) => return Err(e),
        // The producer died without delivering anything — a panic in the closure. The same
        // fail-closed 500 `map_join_error` produced when this handler awaited the JoinHandle;
        // see that function for why the detail is fixed rather than forwarded.
        Err(_) => {
            return Err(ApiError::FailClosed(
                "the viewport producer terminated before its first flush".to_string(),
            ))
        }
    };

    let pin_header = serde_json::to_string(&PinDto::from(&first.stamp))
        .expect("PinDto serialisation cannot fail");

    let response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        // **The content coordinate travels as an entity tag** (`delta-serving.md` §2). The
        // semantics are exactly HTTP's — process my declarations only if this still holds — and
        // contracts §0.2 adopts published formats rather than inventing. One documented deviation:
        // a mismatched `If-Match` does not produce `412`, it produces the full response, because
        // the request remains perfectly answerable and refusing it would turn the fail-closed path
        // into a failure rather than a fallback. Weak-tag syntax is not used; this is an exact
        // comparison of an opaque value.
        .header(
            "etag",
            format!("\"{}\"", hex16(&first.coordinates.content_key)),
        )
        // The authorisation coordinate, which governs whether a held band may be RENDERED at all
        // and is therefore the client's cache PARTITION key. Separate from the entity tag because
        // it answers a different question and moves on a different schedule: HTTP has one
        // validator slot and this is not a validator.
        .header(
            "x-tessera-identity-key",
            hex16(&first.coordinates.identity_key),
        )
        .header("x-tessera-pin", pin_header)
        // The staleness signal (`geometry-pinning.md` §7). A header rather than a body field
        // because the body is framed Arrow IPC and this is one bit that every client — including
        // one that only reads counts — should be able to see without decoding a batch. Always
        // present, so a client never has to distinguish "fresh" from "the server did not say".
        .header("x-tessera-stale", if first.stale { "1" } else { "0" })
        .header("x-tessera-server-us", first.server_us.to_string())
        .header("x-tessera-admission-us", admission_us.to_string());

    // No `x-tessera-stage-ns` header any more: whole-request timings cannot precede the body
    // they describe, so the stage breakdown rides the trailer frame (same double gate).
    Ok(response
        .body(Body::from_stream(StreamBody {
            first: Some(Bytes::from(first.first_frames)),
            rx,
            shared,
            cancel_guard,
            done: false,
        }))
        .expect("response construction cannot fail"))
}

/// Lower-case hex of an opaque 16-byte coordinate. Not a checksum and not reversible by a client:
/// the only operation defined on it is equality against one the server minted earlier.
fn hex16(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The trailer's `stage_ns` value (formerly the `x-tessera-stage-ns` header, which a streamed
/// body cannot carry — whole-request timings cannot precede the body they describe): a
/// fixed-order CSV of unsigned integers, no names.
///
/// **Returns `None` in a build without `bench-timing`**, so a config that turns `stage_timing` on
/// against a release binary emits nothing rather than a row of zeros that reads like a free
/// request path. That is the second of the two gates described on `Config::stage_timing`; the
/// first is the feature on the engine's `Probe`, which leaves every field at zero.
///
/// **I10 / SA §9.** Durations and row counts only — no entity id, no descriptor, no token, no
/// per-principal label. `sigma_visible` and the tile counts are already in the Arrow payload the
/// same response carries, so nothing here is reachable that was not already. What the header does
/// expose is the C4 timing channel in quantified form, which is the point: Appendix C leaves C4
/// open with "quantify before treating as acceptable", and this is the measurement. It is
/// nonetheless off by default and absent from release builds.
///
/// Field order is part of the contract with `scripts/bench_*.py` and `tessera-bench`; append
/// only, never reorder.
///
/// **The per-tile fields are CPU-time sums, not a partition of wall clock.** Once
/// `serve.compute_threads > 1`, `count_ns`, `select_ns`, `gather_ns`, `underlay_ns` and the row
/// counters alongside them are summed across the workers of the parallel tile sweep — see
/// `tessera_engine::StageTimings`'s doc for the full reasoning. A consumer summing this row's
/// duration fields and comparing the total against `x-tessera-server-us` will see the sum run
/// *ahead* of wall time under real parallelism, by roughly the achieved concurrency: that is
/// correct, not a discrepancy to chase. `arrow_serialise_ns` is unaffected — response assembly
/// (this handler) stays serial regardless of the engine's `compute_threads`.
///
/// **That only holds above the calibrated serial fallback.** Below
/// `tessera_engine::viewport::SERIAL_FALLBACK_MAX_ROWS` the tile sweep folds serially at ANY
/// `compute_threads` value — the `pool.install` fan-out does not run at all for those requests —
/// so below that line these per-tile fields do partition the request's own wall clock.
#[cfg(feature = "bench-timing")]
fn stage_header(t: &tessera_engine::StageTimings, arrow_serialise_ns: u64) -> Option<String> {
    // **Append-only.** This is a positional CSV, so inserting a field anywhere but the end silently
    // misaligns every existing consumer — the same reason `served` was appended to the tiles batch
    // rather than slotted next to `visible`. The three trailing fields (theta_anchor_ns,
    // underlay_ns, underlay_cells_evaluated) are therefore out of the durations-then-counters
    // grouping the rest follows; `tessera_bench::report::Stages` is JSON-by-name and keeps the
    // readable order.
    Some(format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
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
    ))
}

#[cfg(not(feature = "bench-timing"))]
fn stage_header(_t: &tessera_engine::StageTimings, _arrow_serialise_ns: u64) -> Option<String> {
    None
}

/// The scalar families, once, for the two places this module walks them: the borrow handed to
/// `viewport_ipc`, and the drill-down's JSON. Two separate matches over thirteen variants is two
/// chances for a type to appear in one of them and not the other.
macro_rules! scalar_families {
    ($mac:ident) => {
        $mac! {
            Bool, U8, U16, U32, U64, I8, I16, I32, I64, F32, F64, TimestampUs,
        }
    };
}

/// Borrow one of the engine's gathered columns as the wire's view of it.
///
/// **The whole of what response assembly now does to the scalar tail.** The engine gathers
/// column-major, so this is a borrow rather than a transpose: what used to be a second full pass
/// over every value — and a `Vec<ScalarOut>` per point to walk it from — is thirteen pointer
/// copies.
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
    /// Optional (contracts §2.2/§2.6): the durable identifier is `external_id`,
    /// so a conforming consumer has no stale `tessera_id` to present in the first place, and
    /// rotation/repartitioning are deliberate breaking changes rather than scheduled hygiene. A
    /// caller that omits this accepts that a `tessera_id` from a past idset may now name a
    /// different item after a repartitioning.
    #[serde(default)]
    idset: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ItemResp {
    /// The full record, by declared column name (records §3): render fields, indexed and
    /// category fields — a category as its vocabulary **key** — and blob-resident fields alike.
    /// An absent field is absent from the object, never `null`: the engine already omits it, and
    /// a `null` would invent a distinction between "no value" and "value of null" that no home
    /// stores.
    fields: serde_json::Map<String, serde_json::Value>,
    /// Base64, present only when the item has a caller-supplied external id. This is the only
    /// place a caller external id appears on the viewer plane — the conformance byte-scanner's
    /// viewer-plane sweep must be scoped to exclude this endpoint's response.
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
}

/// `engine.item`'s sidecar read plus the scalar/external-id shaping that follows it — the CPU-bound
/// and file-IO-bearing part of `/v1/items/{tessera_id}`, run inside `spawn_blocking`.
///
/// ## The three ordering rules this arm structure encodes
///
/// **The idset check runs before inversion and is entity-independent** — identical work and an
/// identical `409` for every presented `tessera_id`, so it opens no channel (contracts §2.2, and C4
/// in the architecture's leak register). It runs *inside* `Engine::item`, against the same
/// generation snapshot that call already loads for the lookup that follows, rather than against a
/// separate `state.engine.meta()` ahead of it: lifecycle §1.1 requires exactly one
/// `generation.load_full()` per request, and a second one for a request that is nominally one
/// lookup breaks it. See [`tessera_engine::Engine::item`]'s doc for the full argument.
///
/// **`404 unknown` is returned identically** for "no such id" and "exists but is not visible to
/// this principal" (contracts §3.2): one `Ok(None)` arm, one `ApiError::Unknown` construction, no
/// branch-dependent logging or metrics anywhere on this path — a second construction site with a
/// different detail string, or a `tracing`/metric call inside only one of the two `None`-shaped
/// cases, would be exactly the oracle this rule exists to prevent.
///
/// **The store-backed `Err` arm can never be reached by anything an attacker chooses.**
/// `Engine::item` inverts `id` (a pure function, no I/O) and tests visibility in entity space — the
/// *same* O(1) work for an id naming nothing and an id naming an invisible item — before it ever
/// touches the external-ID sidecar. A store/IO failure can therefore only be raised for an item
/// already established visible, so a probing client can see a `500` only for an item it can already
/// see; it can never use `500` vs `404` to learn whether an id exists. **A future edit that moves
/// the sidecar read earlier than the visibility test would silently turn this status into a
/// visibility oracle — don't.**
fn run_item(
    state: &AppState,
    session: &tessera_engine::Session,
    raw: u64,
    idset: Option<u32>,
) -> Result<ItemResp, ApiError> {
    let item = match state.engine.item(session, TesseraId::new(raw), idset) {
        // `EngineError::StaleIdSet` -> 409, same fixed detail string as before this was
        // moved inside `Engine::item`. A corrupt or unreadable sidecar (`Store`/`Io`) is a SERVER
        // fault, not "no such item" -- `.ok().flatten()` here would serve a 200 with
        // `external_id: null` and call a digest mismatch a missing field -- fail-open, and
        // precisely what the typed error surface exists to prevent. See this function's doc for
        // why the store-backed arm is unreachable by identifier choice.
        Err(e) => return Err(map_engine_error(e)),
        // Identical 404 for "no such ID" and "exists but not visible". ONE arm, one
        // message, no branch above it -- a second construction site with a different detail
        // string would be the oracle this rule prevents.
        Ok(None) => return Err(ApiError::Unknown("unknown".to_string())),
        Ok(Some(item)) => item,
    };

    let fields = item
        .fields
        .into_iter()
        // Every width lands on a JSON number; the drill-down response is a presentation of the
        // value, not of its storage width, and a client reading `severity: 3` should not have to
        // know the column is a `u8`. The width is a residency decision (per-point-attributes
        // §3.6), and `/v1/meta` publishes it for a client that does care.
        .map(|f| {
            macro_rules! arms {
                ($($v:ident),* $(,)?) => {
                    match f.value {
                        $(tessera_engine::ScalarOut::$v(v) => serde_json::json!(v),)*
                        tessera_engine::ScalarOut::Utf8(v) => serde_json::json!(v),
                    }
                };
            }
            (f.name, scalar_families!(arms))
        })
        .collect();

    let external_id = item
        .external_id
        .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));

    Ok(ItemResp {
        fields,
        external_id,
    })
}

#[derive(Debug, Deserialize)]
struct ArtifactReq {
    /// Which view's row space the count is taken in. **Required, unlike `/v1/items`'s absence of
    /// one**: a point's record is the same wherever it is read from, but a masked count is an
    /// intersection in row space, and row space is per view.
    view: String,
    /// Optional, on [`ItemReq::idset`]'s argument.
    #[serde(default)]
    idset: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ArtifactResp {
    layer: String,
    /// The publisher's own key, if they supplied one. Absent rather than `null` when they did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// **How many of this artifact's members the asking principal can see** — never how many it
    /// has. There is deliberately no ordinal, no membership and no declared size here; see
    /// `tessera_engine::ArtifactOut`.
    masked_count: u64,
    /// The layer's declared derived geometry, recomputed for this principal, in the grid units the
    /// viewport's positions use. Absent where the layer declares none — never *withheld*, since an
    /// artifact whose content could not be served is a `404` (decision 0076).
    #[serde(skip_serializing_if = "Option::is_none")]
    centroid: Option<[f64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#box: Option<[u32; 4]>,
    /// The hull's **rings**, one per separated group of the visible members — a membership that is
    /// two clouds is two rings, not one polygon over the gap between them.
    #[serde(skip_serializing_if = "Option::is_none")]
    hull: Option<Vec<Vec<[u32; 2]>>>,
    /// **The declared resolution this artifact sits at**, the same value the viewport's *artifacts*
    /// frame carries. Always present — every artifact has a level, a treed or flat layer's being 0
    /// — and, unlike everything else here, a fact about the artifact rather than about the asking
    /// principal: two principals served it agree on it.
    level: u32,
    /// The publisher's supplied content — one entry of the ranked `contents`, entire, positional to the layer's declared
    /// kinds. Empty where the layer declares none; never partial, because an artifact whose content
    /// this principal may not read is a `404`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    content: Vec<String>,
}

/// `POST /v1/artifacts/{tessera_id}` — drill down on one artifact.
///
/// **A separate route from `/v1/items`, because they answer about different things.** An item is a
/// document and its record; an artifact is a grouping and one number. Routing both through one
/// endpoint would mean a caller could learn which of the two an identifier names by the *shape* of
/// the answer, and would put a record assembler and a masked count behind one status code.
///
/// **`404` is the only failure shape**, and it is one construction site: an identifier naming
/// nothing, one naming a point, one whose layer this principal cannot reach, one suppressed, and
/// one below its layer's existence criterion are all the same answer with the same detail. That
/// last route reads as new and is not — Appendix C's C17 annotation: the criterion tests the
/// **masked** count, so it can only cross the bar when this principal's own visible membership
/// changes, which is a fact on their own side of the boundary.
///
/// Gated like `/v1/viewport` and `/v1/items`: the work is a mask composition and a bitmap
/// intersection, both cached per session in the steady state, but neither is free on a cold one.
async fn artifact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(raw): AxumPath<u64>,
    Json(req): Json<ArtifactReq>,
) -> Result<Json<ArtifactResp>, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;
    let (gate_permits, _admission_us) = state.compute_gate.admit().await?;

    let served = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        state
            .engine
            .artifact(&entry.session, TesseraId::new(raw), req.idset, &req.view)
            .map_err(crate::error::map_engine_error)
    })
    .await
    .map_err(map_join_error)??;

    // One `None` arm, one construction site, one detail string — a second with different wording,
    // or a log line inside only one of the withheld cases, would be exactly the oracle the single
    // failure shape exists to prevent.
    let served = served.ok_or_else(|| ApiError::Unknown("unknown artifact".to_string()))?;
    Ok(Json(ArtifactResp {
        layer: served.layer,
        key: served.key,
        masked_count: served.masked_count,
        centroid: served.derived.centroid,
        r#box: served.derived.bbox,
        hull: served.derived.hull,
        content: served.content,
        level: served.level,
    }))
}

async fn item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(raw): AxumPath<u64>,
    Json(req): Json<ItemReq>,
) -> Result<Json<ItemResp>, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    // Gated the same way as `/v1/viewport` (see its handler's comment) — `admit()` sheds with 429
    // `backpressure` on either stage.
    let (gate_permits, _admission_us) = state.compute_gate.admit().await?;

    // `engine.item` checks `req.idset` (if the caller sent one) against the ONE generation it
    // loads, inverts the id (pure, no IO), then reads the external-id sidecar for a visible item —
    // file IO, moved off the reactor.
    //
    // The idset check is inside that call rather than here on the reactor ahead of `admit()`,
    // because checking it here would need a second, independent `generation.load_full()` via
    // `state.engine.meta()`, against lifecycle §1.1's one-load-per-request rule. The cost of
    // keeping the rule is that a stale-idset request holds a gate permit for the length of the
    // `spawn_blocking` call rather than being rejected before `admit()` runs; see `Engine::item`'s
    // doc for the full argument.
    //
    // Closure capture: `state` moved in directly (nothing after this `.await` needs the handler's
    // own copy), `entry` moved (already an `Arc<SessionEntry>`), `raw`/`req.idset` are `Copy`,
    // `gate_permits` moves in so both permits release only when this closure returns.
    let resp = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        run_item(&state, &entry.session, raw, req.idset)
    })
    .await
    .map_err(map_join_error)??;

    Ok(Json(resp))
}
