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
//! under `filters`, one more `and_cardinality` per row against a whole-view verdict. On a linked
//! layer a second pass over the served artifacts counts the children of the rows returned,
//! reading each artifact's parent list against the page's handful of rows. **A filter
//! whose leaves route row space is the one whole-view scan this design adds** (§9 (d), owner
//! ruling): a leaf over a render-only column is answered by the viewport only inside its tiles,
//! and browse has none, so the row route's own predicate is run over every row of the view rather
//! than over a request's ranges. Served naively its `matched_count` would be silently zero, which
//! is the failure decision 0104 exists to prevent.
//!
//! A row without text of its own takes its name from an attached label. The label layers are read
//! on the first row that needs one, and a label is looked up only for the rows of a page, or for
//! the candidates of a search whose key does not match, through an index from target to label
//! kept with the label level's row form, so a page does not walk the label layer.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tessera_types::layer::{HierarchyKind, RegisteredLayer};
use tessera_types::{EntityId, TesseraId};

use crate::artifacts::{ArtifactVerdict, ArtifactView};
use crate::compose::compose;
use crate::compose::EffectiveMask;
use crate::error::Result;
use crate::filter::FilterExpr;
use crate::layer_read::{check_level, LayerRefusal, ReadLevel};
use crate::viewport::{response_rungs, segments_with_row_bases, DependencyContext};
use crate::EngineError;

/// Which of §4's three forms a request takes. One verb, three forms (§9 (a), owner ruling).
#[derive(Debug, Clone)]
pub enum BrowseForm {
    /// `parent` and `q` absent: the layer's artifacts with **no served parent**.
    Roots,
    /// `parent` given: the artifacts naming it among their parents, with the requested artifact's
    /// own served parents beside them.
    Children(TesseraId),
    /// `q` given: the artifacts whose key or [`BrowseRow::name`] contains `q` case-insensitively.
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
    /// The artifact's first supplied text content where it has one, and otherwise the first text
    /// of an artifact attached to it that `POST /v1/artifacts` would serve this principal. Label
    /// layers are taken in the order `/v1/meta` lists layers; within one, by level, then keyed
    /// artifacts by key, then keyless ones by publication, which is the order the viewport's
    /// frame serves a level in and the same for a built level and a published one. `None` where
    /// neither gives text.
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
    /// How many artifacts this principal is served that name this one among their parents: the
    /// size of this artifact's children form, on the same rule as `parent_ids`.
    pub child_count: u64,
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
        let refused = |refusal| {
            EngineError::BrowseRefused(match refusal {
                LayerRefusal::Unknown => BrowseRefused::UnknownLayer(req.layer.to_string()),
                LayerRefusal::OneLevel(kind) => BrowseRefused::Level {
                    layer: req.layer.to_string(),
                    detail: format!(
                        "its kind is '{}', which has one level, so `level` names nothing. \
                         Omit it",
                        kind_name(kind)
                    ),
                },
                LayerRefusal::NoSuchLevel { held } => BrowseRefused::Level {
                    layer: req.layer.to_string(),
                    detail: format!(
                        "it holds {held} level(s), so level {} names nothing — /v1/meta \
                         publishes the level set",
                        req.level.unwrap_or(0)
                    ),
                },
            })
        };
        let layer = self
            .readable_layer(session, &generation, req.layer, view)
            .map_err(refused)?;
        if !layer.declaration.depends_on.is_empty() {
            return Err(EngineError::BrowseRefused(BrowseRefused::AttachedLayer(
                req.layer.to_string(),
            )));
        }
        check_level(&layer, req.level).map_err(refused)?;
        let levelled = matches!(
            layer.declaration.hierarchy.kind,
            HierarchyKind::Stacked | HierarchyKind::Tiered
        );
        let level = req.level.unwrap_or(0);
        let linked = !matches!(
            layer.declaration.hierarchy.kind,
            HierarchyKind::Flat | HierarchyKind::Stacked
        );

        // `session_geometry` laps into a probe; this verb publishes no per-stage timings, so it
        // is given one and its laps are dropped. Named `_probe` rather than silenced afterwards,
        // so that a stage field arriving here is a change to this line and not to a discard.
        let mut _probe = crate::timing::Probe::new();
        let (geometry, _) =
            self.session_geometry(session, &generation, view, view_data, &None, &mut _probe)?;
        let denied = generation
            .denied()
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;
        let mask = compose(
            session.satisfied(),
            &generation.overlay,
            &generation.buffer,
            Arc::clone(&geometry.projection),
            &view_data.row_space,
            denied,
            generation.buffered_rows(view),
        );
        let served_view = crate::viewport::ServedView {
            session,
            generation: &generation,
            name: view,
            data: view_data,
            segments: segments_with_row_bases(view, view_data)?,
            denied,
            mask_identity: self.mask_identity(session, &generation, &geometry),
        };

        // **The filter, evaluated once for the request and over the whole view.** Every route it
        // may take is asked for its whole-view answer: an entity-space verdict is projected whole,
        // a region is whole-view by construction, and a leaf over a render-only column is scanned
        // over every row of the view — the owner's ruling of §9 (d), and the one whole-view pass
        // this design adds.
        let filter_rows = match &req.filter {
            None => None,
            Some(expr) => Some(
                self.whole_view_filter_rows(&served_view, &mask, expr, &None)?
                    .0,
            ),
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
        for (walked, runs) in layer.runs.iter().enumerate() {
            let walked = walked as u32;
            let mut level_read = self.read_level(&served_view, &mask, &layer, walked, false);
            if let Some(filter_rows) = &filter_rows {
                level_read.filtered = level_read.filtered_counts(self, &mask, filter_rows);
            }
            let rows = &level_read.rows;
            let view_of = level_read.view(self, &served_view, &mask, &layer, &|_| false);
            for ordinal in 0..rows.len() as u32 {
                let Some(entity) = runs.entity_of(ordinal as u64).map(EntityId::new) else {
                    continue;
                };
                let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
                    view_of.verdict(entity, ordinal)
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
                let Some(content) =
                    level_read.content(self, &generation, &layer, ordinal, entity, rank)
                else {
                    continue;
                };
                if let Some(filter_rows) = filter_rows.as_ref() {
                    filtered.insert(
                        (walked, ordinal),
                        level_read.matched_count(ordinal, &mask, filter_rows),
                    );
                }
                names.insert(
                    (walked, ordinal),
                    content
                        .first_text()
                        .filter(|text| !text.is_empty())
                        .map(str::to_string),
                );
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

        // **The layers attached to this one that this principal reads**, each level under this
        // request's mask, and each artifact of them judged by the viewport's own verdict with the
        // viewport's dependency hook. They are read on the first row that has no text of its own,
        // and a name is looked up only for a row that needs one: the rows of a page, and the
        // candidates of a search whose key does not match.
        let reachable = self.reachable_layers(session);
        let ctx = DependencyContext::new(&served_view, &mask, &reachable);
        let dependency_served = self.dependency_gate(&ctx);
        let label_layers: OnceCell<Vec<(RegisteredLayer, Vec<ReadLevel>)>> = OnceCell::new();
        let label_views: OnceCell<Vec<Vec<ArtifactView<'_, EffectiveMask>>>> = OnceCell::new();
        let attached_name = |level: u32, ordinal: u32| -> Option<String> {
            let target = layer.runs[level as usize]
                .entity_of(u64::from(ordinal))
                .map(EntityId::new)?;
            let layers = label_layers.get_or_init(|| {
                reachable
                    .names()
                    .filter_map(|name| self.readable_layer(session, &generation, name, view).ok())
                    .filter(|label| label.declaration.depends_on.iter().any(|d| d == req.layer))
                    .map(|label| {
                        let levels = (0..label.runs.len() as u32)
                            .map(|level| self.read_level(&served_view, &mask, &label, level, false))
                            .collect();
                        (label, levels)
                    })
                    .collect()
            });
            let views = label_views.get_or_init(|| {
                layers
                    .iter()
                    .map(|(label, levels)| {
                        levels
                            .iter()
                            .map(|read| {
                                read.view(self, &served_view, &mask, label, &dependency_served)
                            })
                            .collect()
                    })
                    .collect()
            });
            layers
                .iter()
                .zip(views)
                .find_map(|((label, levels), views)| {
                    levels.iter().zip(views).find_map(|(read, view)| {
                        // An edge naming an entity the target slot no longer holds is into an
                        // artifact since republished over.
                        let mut attached: Vec<u32> = read
                            .rows
                            .records()
                            .attached_to(req.layer, level, ordinal)
                            .filter(|&at| {
                                read.rows.attachment(at).is_some_and(|a| a.entity == target)
                            })
                            .collect();
                        // The viewport's order within a level: by key, then the keyless by ordinal.
                        if attached.len() > 1 {
                            let name = label.declaration.name.as_str();
                            let keyed: Vec<(Option<String>, u32)> =
                                self.write.live().with_artifacts(|store| {
                                    attached
                                        .iter()
                                        .map(|&at| {
                                            (
                                                store
                                                    .get(name, read.level, at)
                                                    .and_then(|r| r.key.clone()),
                                                at,
                                            )
                                        })
                                        .collect()
                                });
                            let mut keyed = keyed;
                            keyed.sort_unstable_by(|(a, at), (b, bt)| {
                                (a.is_none(), a, at).cmp(&(b.is_none(), b, bt))
                            });
                            attached = keyed.into_iter().map(|(_, at)| at).collect();
                        }
                        attached.into_iter().find_map(|at| {
                            let entity = label.runs[read.level as usize]
                                .entity_of(u64::from(at))
                                .map(EntityId::new)?;
                            let ArtifactVerdict::Serve { rank, .. } = view.verdict(entity, at)
                            else {
                                return None;
                            };
                            read.content(self, &generation, label, at, entity, rank)?
                                .first_text()
                                .filter(|text| !text.is_empty())
                                .map(str::to_string)
                        })
                    })
                })
        };
        let resolved: RefCell<HashMap<(u32, u32), Option<String>>> = RefCell::default();
        let name_of = |g: &Gated| -> Option<String> {
            let at = (g.level, g.ordinal);
            if let Some(name) = resolved.borrow().get(&at) {
                return name.clone();
            }
            let name = names
                .get(&at)
                .cloned()
                .flatten()
                .or_else(|| attached_name(g.level, g.ordinal));
            resolved.borrow_mut().insert(at, name.clone());
            name
        };

        let row_of = |g: &Gated, children: &HashMap<(u32, u32), u64>| BrowseRow {
            tessera_id: g.tessera_id,
            key: keys.get(&(g.level, g.ordinal)).cloned().flatten(),
            name: name_of(g),
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
            child_count: children
                .get(&(g.level, g.ordinal))
                .copied()
                .unwrap_or(0),
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
                            let found = |text: Option<String>| {
                                text.is_some_and(|text| text.to_lowercase().contains(&needle))
                            };
                            g.level == level
                                && (found(keys.get(&(g.level, g.ordinal)).cloned().flatten())
                                    || found(name_of(g)))
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

        // The children of each row returned, counted over the gated artifacts, so both ends are
        // served. A child counts once however many times its parent list repeats the parent, as
        // the children form lists it once. A `flat` or `stacked` layer has no links to count.
        let mut returned: Vec<Vec<bool>> = vec![Vec::new(); layer.runs.len()];
        for &at in page.iter().chain(&parents) {
            let marks = &mut returned[gated[at].level as usize];
            let ordinal = gated[at].ordinal as usize;
            if marks.len() <= ordinal {
                marks.resize(ordinal + 1, false);
            }
            marks[ordinal] = true;
        }
        let is_returned = |&(level, ordinal): &(u32, u32)| {
            returned
                .get(level as usize)
                .and_then(|marks| marks.get(ordinal as usize))
                .copied()
                .unwrap_or(false)
        };
        let mut children: HashMap<(u32, u32), u64> = HashMap::new();
        if linked && !(page.is_empty() && parents.is_empty()) {
            for g in &gated {
                for (i, at) in g.parents.iter().enumerate() {
                    if is_returned(at) && !g.parents[..i].contains(at) {
                        *children.entry(*at).or_default() += 1;
                    }
                }
            }
        }
        let mut parent_rows: Vec<BrowseRow> = parents
            .iter()
            .map(|&at| row_of(&gated[at], &children))
            .collect();
        parent_rows.sort_by_key(|row| row.tessera_id.raw());
        parent_rows.dedup_by_key(|row| row.tessera_id.raw());
        Ok(BrowseOut {
            artifacts: page
                .iter()
                .map(|&at| row_of(&gated[at], &children))
                .collect(),
            parents: parent_rows,
            next,
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
