//! **A layer's hierarchy by lineage, independent of the viewport** — `POST /v1/artifacts/browse`
//! (`highlight-and-hierarchy.md` §4).
//!
//! Rung 3 put a 30,217-descriptor DAG into a bundle and the viewport could show none of it: a
//! MeSH descriptor's members are spread over the whole layout, so the budget cut deepens uniformly
//! and the leaves arrive at a zoom nobody reaches. This verb serves the same artifacts **by
//! relation instead of by position** — the layer's roots, one artifact's children and parents, and
//! a name search — each row carrying the masked count the artifacts frame carries.
//!
//! # What makes it safe, in four sentences
//!
//! - **The gate is the viewport's own**, run through [`crate::artifacts::ArtifactView::verdict`]
//!   for every artifact of the layer, and run **before** `limit` and `cursor` are applied — so a
//!   page's fill and its `next` count only artifacts this principal may see, and a withheld
//!   artifact leaves no gap a caller could count.
//! - **Every count is C8's `and_cardinality` against `M_auth`**, computed per request: the masked
//!   count is the artifacts frame's, and the filtered one is `|membership ∩ M_auth ∩ filter|`
//!   against a verdict the filter contract's own routes produced inside `M_auth`.
//! - **Existence and `masked_count` never move with the filter** (**I3**, **I12**) — a row whose
//!   `matched_count` is zero is still served, exactly as an artifact whose `matched` bit is false
//!   is still served on the viewport.
//! - **A relation is named only where both ends are served** (C29, per entry): a child whose
//!   parent is withheld is a root here, a parent below its floor is absent from `parents`, and a
//!   requested `parent` that fails its own criterion answers an empty page identically to a leaf.
//!
//! The register row is **C33**, and its argument is that enumeration was already available:
//! `artifact_budget` is a request bound and never a disclosure control, so a zoom-0 viewport at a
//! large budget with every level named serves the same set. This verb serves it paged and by
//! relation instead of by position.
//!
//! # What it costs
//!
//! One pass over the layer's artifacts — the gate, and one masked `and_cardinality` each — plus,
//! under `filters`, one more `and_cardinality` per row against a whole-view verdict. **A filter
//! whose leaves route row space is the one whole-view scan this design adds** (§9 (d), owner
//! ruling): a leaf over a render-only column is answered by the viewport only inside its tiles,
//! and browse has none, so the row route's own predicate is run over every row of the view rather
//! than over a request's ranges. Served naively its `matched_count` would be silently zero, which
//! is the failure decision 0104 exists to prevent.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use croaring::Bitmap;

use tessera_types::layer::HierarchyKind;
use tessera_types::{EntityId, TesseraId};

use crate::compose::{compose, MaskedSet, WholeMask};
use crate::filter::{FilterExpr, RoutedFilter};
use crate::viewport::{response_rungs, segments_with_row_bases};
use crate::session::Result;
use crate::EngineError;

/// Which of §4's three forms a request takes. One verb, three forms (§9 (a), owner ruling).
#[derive(Debug, Clone)]
pub enum BrowseForm {
    /// `parent` and `q` absent: the layer's artifacts with **no served parent**.
    Roots,
    /// `parent` given: the artifacts naming it among their parents, with the requested artifact's
    /// own served parents beside them.
    Children(TesseraId),
    /// `q` given: the artifacts whose key, or whose first supplied text content, contains `q`
    /// case-insensitively.
    Search(String),
}

/// One `POST /v1/artifacts/browse` request.
#[derive(Debug, Clone)]
pub struct BrowseRequest<'a> {
    pub view: &'a str,
    pub layer: &'a str,
    /// Which level the form addresses on a **levelled** layer (`stacked`, `tiered`); a `422` on
    /// the three one-level kinds, a kind's levels being deployment schema and a parameter accepted
    /// and ignored being a wrong answer that looks right (§4).
    pub level: Option<u32>,
    pub form: BrowseForm,
    /// The viewport's own `filters` object. Its answer here is a **count per row** rather than the
    /// viewport's boolean, on §4's argument: browse has no tiles and no held payload, so 0104's
    /// two objections — a tile-scoped number beside a whole-view one, and a filter change having
    /// to move one bit of a held row — do not apply.
    pub filter: Option<FilterExpr>,
    /// Clamped to `selection.max_browse_rows`; `0` is a `422` on `/v1/categories`' argument (a
    /// zero-length page is a question with no answer).
    pub limit: usize,
    pub cursor: Option<BrowseCursor>,
}

/// A position in the total order — `(masked or matched count descending, tessera_id ascending)`.
///
/// **The order is total**, which is what lets a cursor page over tied counts without duplicating
/// or dropping a row: two artifacts with the same count are separated by their identifiers, and no
/// two artifacts share one.
///
/// It carries a count this principal was already served and an identifier they were already
/// handed, so it discloses nothing a page did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowseCursor {
    pub count: u64,
    pub tessera_id: u64,
}

impl BrowseCursor {
    /// The wire spelling: `<count>:<tessera_id>`, both decimal.
    pub fn parse(text: &str) -> Option<BrowseCursor> {
        let (count, id) = text.split_once(':')?;
        Some(BrowseCursor {
            count: count.parse().ok()?,
            tessera_id: id.parse().ok()?,
        })
    }

    pub fn encode(&self) -> String {
        format!("{}:{}", self.count, self.tessera_id)
    }
}

/// One row of a browse page: the artifacts frame's identity row plus a name and the counts (§4).
///
/// **No geometry.** This verb serves a hierarchy; a layer that draws is drawn by the viewport.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowseRow {
    pub tessera_id: TesseraId,
    /// The publisher's own key, where they supplied one.
    pub key: Option<String>,
    /// The artifact's **first supplied text content**, where this principal may read it — the
    /// containment rule's own answer, so a viewer who holds no content's generating set entire is
    /// served no artifact at all rather than this field empty.
    pub name: Option<String>,
    /// `|membership ∩ M_auth|`, computed per request and never precomputed (C8). **It never moves
    /// with the filter.**
    pub masked_count: u64,
    /// `|membership ∩ M_auth ∩ filter|`, or `None` where the request carried no `filters` — *there
    /// was no question*, exactly as the artifacts frame's `matched` is null then.
    pub matched_count: Option<u64>,
    /// The declared level on a levelled layer; the depth of this artifact in the **gated** forest
    /// on a treed one, which is the same definition the viewport applies over its own response
    /// read over the whole layer; `0` on a flat layer.
    pub rung: u32,
    /// This artifact's parents **that this principal is also served**, ascending — C29 per entry,
    /// exactly as the artifacts frame's list is.
    pub parent_ids: Vec<TesseraId>,
}

/// One browse page.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowseOut {
    pub artifacts: Vec<BrowseRow>,
    /// The requested artifact's own served parents — **present on the children form only**, and
    /// `[]` on the others rather than absent, so a client reads one shape.
    pub parents: Vec<BrowseRow>,
    /// The cursor for the next page, or `None` where this page is the last.
    pub next: Option<BrowseCursor>,
}

/// Why a browse request could not be answered. Every one is the caller's fault and a `422` — each
/// names deployment schema the caller reads off `/v1/meta`, so refusing discloses nothing.
///
/// **What is deliberately absent is a refusal about an artifact.** A `parent` that names nothing,
/// one of another layer, one suppressed and one below this principal's own criterion all answer an
/// empty page, on the same rule §3's `member_of` leaf follows: refusing would make the verb an
/// existence oracle over exactly what the criterion withholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseRefused {
    /// A layer outside the list `/v1/meta` publishes to this principal. The registry's probe
    /// answers alike for a gate-failed name and a never-registered one, so this confirms nothing.
    UnknownLayer(String),
    /// A layer that hangs from another has no lineage of its own and its text is what its target
    /// shows as a name, so it is not browsed separately (§4).
    AttachedLayer(String),
    /// `level` on a kind with one level (`flat`, `nested`, `dag`), or a level this layer does not
    /// hold. A kind's levels are deployment schema, and a parameter accepted and ignored is a
    /// wrong answer that looks right.
    Level { layer: String, detail: String },
    /// `limit = 0`: a zero-length page is a question with no answer.
    ZeroLimit,
}

impl std::fmt::Display for BrowseRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowseRefused::UnknownLayer(layer) => write!(
                f,
                "'{layer}' is not a layer this deployment publishes to you — /v1/meta lists the \
                 layers that can be browsed"
            ),
            BrowseRefused::AttachedLayer(layer) => write!(
                f,
                "layer '{layer}' attaches to another, so it has no lineage of its own and its text \
                 is what its target shows as a name. Browse the layer it hangs from"
            ),
            BrowseRefused::Level { layer, detail } => {
                write!(f, "layer '{layer}': {detail}")
            }
            BrowseRefused::ZeroLimit => write!(
                f,
                "`limit` is 0, which asks for a page with no rows. Omit it for the deployment's \
                 default, or name a positive number up to `selection.max_browse_rows`"
            ),
        }
    }
}

/// One artifact of one level, as the gate left it.
///
/// **Its entity and its content rank are consumed inside the walk and not carried out of it**: the
/// rank decides which content this viewer is served, and both are answered before the row is
/// pushed. Keeping them here would be state a later reader could reach a second verdict from.
struct Gated {
    level: u32,
    ordinal: u32,
    masked_count: u64,
    tessera_id: TesseraId,
    parents: Vec<(u32, u32)>,
}

impl crate::Engine {
    /// Serve one page of a layer's hierarchy — see this module's doc.
    pub fn browse(&self, session: &crate::Session, req: BrowseRequest<'_>) -> Result<BrowseOut> {
        if req.limit == 0 {
            return Err(EngineError::BrowseRefused(BrowseRefused::ZeroLimit));
        }
        let generation = self.generation.load_full();
        let view = req.view;
        let carriers = generation
            .bundle
            .partitions
            .values()
            .filter(|partition| partition.views.contains_key(view))
            .count();
        if carriers > 1 {
            return Err(EngineError::MultiPartitionView(view.to_string()));
        }
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;

        // **The layer, resolved through the principal's own reachable set** — one probe for a
        // gate-failed name and a never-registered one alike, so a `422` here confirms nothing this
        // principal was not already told by `/v1/meta`.
        let refuse_layer = || EngineError::BrowseRefused(BrowseRefused::UnknownLayer(req.layer.to_string()));
        let reachable = self.write.live().resolve_layers(
            |term| session.satisfied.contains(&term),
            |label| generation.dict.lookup(label.as_bytes()),
        );
        if !reachable.contains(req.layer) {
            return Err(refuse_layer());
        }
        let layer = self.write.live().registered_layer(req.layer).ok_or_else(refuse_layer)?;
        if !layer.declaration.views.iter().any(|s| s == view)
            || generation.overlay.is_deleted(layer.entity)
            || generation.overlay.is_suppressed(layer.entity)
        {
            return Err(refuse_layer());
        }
        if !layer.declaration.depends_on.is_empty() {
            return Err(EngineError::BrowseRefused(BrowseRefused::AttachedLayer(
                req.layer.to_string(),
            )));
        }

        // **`level` is refused on a one-level kind and clamped by nothing.** `flat`, `nested` and
        // `dag` sit entirely at level 0 (decision 0082), so a level parameter there names a
        // resolution the layer does not have.
        let levelled = matches!(
            layer.declaration.hierarchy.kind,
            HierarchyKind::Stacked | HierarchyKind::Tiered
        );
        if let Some(level) = req.level {
            if !levelled {
                return Err(EngineError::BrowseRefused(BrowseRefused::Level {
                    layer: req.layer.to_string(),
                    detail: format!(
                        "its kind is '{}', which has one level, so `level` names nothing. \
                         Omit it",
                        kind_name(layer.declaration.hierarchy.kind)
                    ),
                }));
            }
            if level as usize >= layer.runs.len() {
                return Err(EngineError::BrowseRefused(BrowseRefused::Level {
                    layer: req.layer.to_string(),
                    detail: format!(
                        "it holds {} level(s), so level {level} names nothing — /v1/meta \
                         publishes the level set",
                        layer.runs.len()
                    ),
                }));
            }
        }
        let level = req.level.unwrap_or(0);

        // `session_geometry` laps into a probe; this verb publishes no per-stage timings, so it
        // is given one and its laps are dropped. Named `_probe` rather than silenced afterwards,
        // so that a stage field arriving here is a change to this line and not to a discard.
        let mut _probe = crate::timing::Probe::new();
        let geometry =
            self.session_geometry(session, &generation, view, view_data, &None, &mut _probe)?;
        let denied = generation
            .denied()
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;
        let mask = compose(
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            Arc::clone(&geometry.projection),
            &view_data.row_space,
            denied,
        );
        let segments = segments_with_row_bases(view, view_data)?;
        let mask_identity = self.mask_identity(session, &generation, &geometry);

        // **The filter, evaluated once for the request and over the whole view.** Every route it
        // may take is asked for its whole-view answer: an entity-space verdict is projected whole,
        // a region is whole-view by construction, and a leaf over a render-only column is scanned
        // over every row of the view — the owner's ruling of §9 (d), and the one whole-view pass
        // this design adds.
        let filter_rows = match &req.filter {
            None => None,
            Some(expr) => Some(self.browse_filter_rows(
                session,
                &generation,
                view,
                view_data,
                &segments,
                &mask,
                denied,
                mask_identity,
                expr,
            )?),
        };

        // **The gate, over every artifact of every level of the layer, before any page.** Every
        // form needs the whole gated set: roots need to know which parents are served, children
        // may sit at another level, and a page's `next` must count only rows this principal may
        // see. The cost is the layer's artifact count, which §7 prices and which is what bounds
        // this verb rather than the corpus.
        let mut gated: Vec<Gated> = Vec::new();
        let mut names: HashMap<(u32, u32), Option<String>> = HashMap::new();
        let mut keys: HashMap<(u32, u32), Option<String>> = HashMap::new();
        let mut filtered: HashMap<(u32, u32), u64> = HashMap::new();
        let shard = generation.bundle.manifest.identity.shard_id;
        let source = generation.partition_source();
        let vocabulary = crate::viewport::predicate_vocabulary(&generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let wants_name = !layer.declaration.content.supplied.is_empty();
        for (walked, runs) in layer.runs.iter().enumerate() {
            let walked = walked as u32;
            let recorded = layer.layout_of(walked);
            let (rows, level_version) = self.write.live().with_artifacts(|store| {
                let predicate = crate::viewport::predicate_source(
                    &layer.declaration,
                    &generation,
                    view,
                    view_data,
                    &segments,
                    &code_of_key,
                    &self.shapes,
                    store,
                    walked,
                );
                // The version the form is of, not the store's — see
                // `ArtifactProjections::get_or_build`. The histogram below is filed under it and
                // is the same entry a viewport reads.
                self.artifact_projections.get_or_build(
                    &generation.prefix,
                    view,
                    req.layer,
                    walked,
                    store,
                    &view_data.row_space,
                    Some(&source),
                    recorded,
                    predicate.as_ref(),
                    generation.segments_version,
                    crate::artifacts::serves_column_only(&layer.declaration),
                )
            });
            let counts = self.masked_counts(
                &mask_identity,
                view,
                req.layer,
                walked,
                level_version,
                &rows,
                &mask,
                // A browse page carries a name and a count, never a derived geometry.
                None,
            );
            let containment = rows.partition().map(|p| p.answers(&session.satisfied));
            let view_of = crate::artifacts::ArtifactView {
                declaration: &layer.declaration,
                overlay: &generation.overlay,
                satisfied: &session.satisfied,
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                dependency_served: &|_| false,
                containment,
                denied,
                counts,
            };
            // The row-major route's filtered counts, taken in one walk of `M_auth ∩ filter` — the
            // same shape the masked histogram takes, over a narrower set. A label column has no
            // per-artifact membership to intersect, so this is the only route to the number.
            let filtered_histogram = match (rows.column(), filter_rows.as_ref()) {
                (Some(column), Some(rows_of_filter)) => {
                    // Narrowed into a copy, because the composed set is the mask's and is borrowed.
                    // The count beside an artifact is filter-blind (I12) and the whole-map
                    // candidacy route reads the same set, so narrowing it in place would make both
                    // a function of this request's filter.
                    let visible = mask.visible_all().and(rows_of_filter);
                    // The engine's pool, for `Engine::masked_counts`' reason: the walk splits.
                    Some(self.pool.install(|| column.histogram_over(&visible)))
                }
                _ => None,
            };
            let contents = if wants_name {
                Some(self.level_contents.get_or_build(
                    req.layer,
                    walked,
                    level_version,
                    generation.segments_version,
                    || {
                        crate::artifact_content::LevelContent::build(
                            generation.filter_columns.records(),
                            runs,
                        )
                    },
                ))
            } else {
                None
            };
            for ordinal in 0..rows.len() as u32 {
                let Some(entity) = runs.entity_of(ordinal as u64).map(EntityId::new) else {
                    continue;
                };
                // ⊘ **No artifact carries its own terms yet**, so a layer whose
                // `artifact_visibility` names a field serves nothing here — the same fail-closed
                // answer the viewport gives, reached by the same call.
                let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
                    view_of.verdict(entity, ordinal, None)
                else {
                    continue;
                };
                let Ok(tessera_id) = self.identity_key.forward(shard, entity) else {
                    continue;
                };
                // Content decides servability as well as text: a viewer containing no content's
                // generating set receives no artifact at all rather than one with its description
                // missing (decision 0076), which is why this runs inside the gate and not beside
                // the row build.
                let Some(content) = self.supplied_content(
                    &generation,
                    req.layer,
                    walked,
                    ordinal,
                    entity,
                    layer.declaration.content.supplied.len(),
                    rank,
                    true,
                    contents.as_deref(),
                ) else {
                    continue;
                };
                if let Some(filter_rows) = filter_rows.as_ref() {
                    let count = match (&filtered_histogram, rows.get(ordinal)) {
                        (Some(histogram), _) => {
                            histogram.get(ordinal as usize).copied().unwrap_or(0) as u64
                        }
                        (None, Some(members)) => {
                            mask.visible_rows(members).and_cardinality(filter_rows)
                        }
                        (None, None) => 0,
                    };
                    filtered.insert((walked, ordinal), count);
                }
                names.insert((walked, ordinal), content.into_iter().next());
                keys.insert(
                    (walked, ordinal),
                    self.write
                        .live()
                        .with_artifacts(|store| store.get(req.layer, walked, ordinal)?.key.clone()),
                );
                gated.push(Gated {
                    level: walked,
                    ordinal,
                    masked_count,
                    tessera_id,
                    parents: rows
                        .parents(ordinal)
                        .iter()
                        .map(|p| (p.level, p.ordinal))
                        .collect(),
                });
            }
        }
        // Every served position, so a relation can be named only where both ends are served.
        let served: BTreeMap<(u32, u32), usize> = gated
            .iter()
            .enumerate()
            .map(|(at, g)| ((g.level, g.ordinal), at))
            .collect();

        // **The rung.** A levelled layer's is its declared level — a fact about the artifact. A
        // treed layer declares no levels and sits at level 0 (decision 0082), so its rung is the
        // depth of this artifact in the forest the *served* parent links form, which is the
        // viewport's own definition read over the whole gated layer rather than over one response.
        let rungs = if levelled {
            HashMap::new()
        } else {
            let parents_of: HashMap<u64, Vec<u64>> = gated
                .iter()
                .map(|g| {
                    (
                        g.tessera_id.raw(),
                        g.parents
                            .iter()
                            .filter_map(|at| served.get(at).map(|&i| gated[i].tessera_id.raw()))
                            .collect(),
                    )
                })
                .collect();
            response_rungs(&parents_of)
        };

        let row_of = |g: &Gated| BrowseRow {
            tessera_id: g.tessera_id,
            key: keys.get(&(g.level, g.ordinal)).cloned().flatten(),
            name: names.get(&(g.level, g.ordinal)).cloned().flatten(),
            masked_count: g.masked_count,
            matched_count: filtered.get(&(g.level, g.ordinal)).copied(),
            rung: if levelled {
                g.level
            } else {
                rungs.get(&g.tessera_id.raw()).copied().unwrap_or(0)
            },
            parent_ids: {
                let mut ids: Vec<TesseraId> = g
                    .parents
                    .iter()
                    .filter_map(|at| served.get(at).map(|&i| gated[i].tessera_id))
                    .collect();
                ids.sort_unstable_by_key(|id| id.raw());
                ids.dedup();
                ids
            },
        };

        // The form's own candidate set, taken over the gated artifacts and never over the level's
        // population.
        let (candidates, parents): (Vec<usize>, Vec<usize>) = match &req.form {
            BrowseForm::Roots => (
                gated
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| {
                        g.level == level && !g.parents.iter().any(|at| served.contains_key(at))
                    })
                    .map(|(at, _)| at)
                    .collect(),
                Vec::new(),
            ),
            BrowseForm::Children(id) => {
                // The requested artifact, under its own criterion. One that fails it — or names
                // nothing, or belongs to another layer — answers an empty page identically to a
                // leaf: this is §3's empty-operand rule, one verb over.
                let Some(&at) = gated
                    .iter()
                    .position(|g| g.tessera_id == *id)
                    .as_ref()
                    .map(|at| at as &usize)
                else {
                    return Ok(BrowseOut {
                        artifacts: Vec::new(),
                        parents: Vec::new(),
                        next: None,
                    });
                };
                let key = (gated[at].level, gated[at].ordinal);
                (
                    gated
                        .iter()
                        .enumerate()
                        .filter(|(_, g)| g.parents.contains(&key))
                        .map(|(i, _)| i)
                        .collect(),
                    gated[at]
                        .parents
                        .iter()
                        .filter_map(|p| served.get(p).copied())
                        .collect(),
                )
            }
            BrowseForm::Search(q) => {
                let needle = q.to_lowercase();
                (
                    gated
                        .iter()
                        .enumerate()
                        .filter(|(_, g)| {
                            g.level == level
                                && [
                                    keys.get(&(g.level, g.ordinal)).cloned().flatten(),
                                    names.get(&(g.level, g.ordinal)).cloned().flatten(),
                                ]
                                .iter()
                                .flatten()
                                .any(|text| text.to_lowercase().contains(&needle))
                        })
                        .map(|(at, _)| at)
                        .collect(),
                    Vec::new(),
                )
            }
        };

        // **The total order**: count descending, then `tessera_id` ascending, so a cursor over tied
        // counts neither duplicates nor drops a row. The count is the filtered one under `filters`
        // and the masked one otherwise (§4).
        let sort_key = |at: &usize| {
            let g = &gated[*at];
            let count = filtered
                .get(&(g.level, g.ordinal))
                .copied()
                .unwrap_or(g.masked_count);
            (std::cmp::Reverse(count), g.tessera_id.raw())
        };
        let mut candidates = candidates;
        candidates.sort_by_key(sort_key);
        let after = match req.cursor {
            None => 0,
            Some(cursor) => candidates.partition_point(|at| {
                sort_key(at) <= (std::cmp::Reverse(cursor.count), cursor.tessera_id)
            }),
        };
        let page: Vec<usize> = candidates
            .iter()
            .skip(after)
            .take(req.limit)
            .copied()
            .collect();
        let next = (after + page.len() < candidates.len())
            .then(|| {
                page.last().map(|at| {
                    let g = &gated[*at];
                    BrowseCursor {
                        count: filtered
                            .get(&(g.level, g.ordinal))
                            .copied()
                            .unwrap_or(g.masked_count),
                        tessera_id: g.tessera_id.raw(),
                    }
                })
            })
            .flatten();

        let mut parent_rows: Vec<BrowseRow> =
            parents.iter().map(|&at| row_of(&gated[at])).collect();
        parent_rows.sort_by_key(|row| row.tessera_id.raw());
        parent_rows.dedup_by_key(|row| row.tessera_id.raw());
        Ok(BrowseOut {
            artifacts: page.iter().map(|&at| row_of(&gated[at])).collect(),
            parents: parent_rows,
            next,
        })
    }

    /// The request's filter as a **whole-view** row set — see this module's doc on §9 (d).
    #[allow(clippy::too_many_arguments)]
    fn browse_filter_rows(
        &self,
        session: &crate::Session,
        generation: &crate::Generation,
        view: &str,
        view_data: &tessera_store::read::ViewData,
        segments: &[(&tessera_store::read::SegmentData, u32)],
        mask: &crate::compose::EffectiveMask,
        denied: &Bitmap,
        mask_identity: crate::histogram::MaskIdentity,
        expr: &FilterExpr,
    ) -> Result<Bitmap> {
        let fragment = self.fragment_for(session, generation)?;
        let candidate = crate::filter::candidate(
            &fragment,
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
        );
        // **`Engine::browse` takes no cancellation token, and could not usefully hold one**: the
        // row route's `scan_rows` carries no checkpoint on any path — the viewport's own
        // coarse-zoom whole-view scan is equally uninterruptible — and this is a unary JSON verb
        // with no consumer-gone signal of the kind the streamed viewport reads off its sink. So
        // the scan is **admission-gated and not cancellable**, and the permit is held for its
        // duration (`highlight-and-hierarchy.md` §7, which prices that). Making it interruptible
        // is a change to the shared row route rather than to this verb.
        let cancel = None;
        let regions = |leaf: &crate::filter::RegionLeaf| {
            self.resolve_region(
                leaf,
                session,
                generation,
                view,
                view_data,
                segments,
                mask,
                denied,
                mask_identity,
                &cancel,
            )
        };
        let members = |leaf: &crate::filter::MemberOfLeaf| {
            self.resolve_member_of(
                leaf,
                session,
                generation,
                view,
                view_data,
                segments,
                mask,
                denied,
                mask_identity,
            )
        };
        let resolvers = crate::filter::RowLeafResolvers {
            regions: &regions,
            members: &members,
        };
        let total_rows = view_data.row_space.total_rows();
        // `prefer_row = false`: browse has no tile ranges to make the row route cheaper, so a
        // both-routes column takes the entity route and only a render-only column reaches the scan.
        let routed = generation
            .filter_columns
            .evaluate_routed(expr, &candidate, false, &resolvers)
            .map_err(|e| {
                let detail = e.to_string();
                if e.is_callers_fault() {
                    EngineError::FilterMalformed(detail)
                } else {
                    EngineError::FilterRefused(detail)
                }
            })?;
        Ok(match routed {
            RoutedFilter::Entity(entities) => view_data.row_space.project(&entities),
            RoutedFilter::Row(tree) => {
                // The whole view as the one domain: the row route's predicate runs over every row
                // rather than over a request's ranges, which is what makes the count exact off the
                // viewport and is the cost §7 prices.
                let whole = std::slice::from_ref(&std::ops::Range {
                    start: 0u32,
                    end: u32::try_from(total_rows).unwrap_or(u32::MAX),
                })
                .to_vec();
                self.evaluate_row_route(
                    &tree,
                    &view_data.row_space,
                    segments,
                    &whole,
                    total_rows,
                    total_rows,
                    false,
                )?
                .rows()
                .clone()
            }
        })
    }
}

fn kind_name(kind: HierarchyKind) -> &'static str {
    match kind {
        HierarchyKind::Flat => "flat",
        HierarchyKind::Nested => "nested",
        HierarchyKind::Dag => "dag",
        HierarchyKind::Stacked => "stacked",
        HierarchyKind::Tiered => "tiered",
    }
}
