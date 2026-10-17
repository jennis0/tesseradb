//! Serving annotation artifacts: the gate, the dependency prerequisite and the response pass.

use super::*;

/// The layers a request names that this principal reaches, in the request's order. Intersected
/// with the request, never unioned: asking for a name is not a way to learn it.
pub(super) fn requested_layers(
    selection: LayerSelection<'_>,
    reachable: &mosaica_lifecycle::ResolvedLayers,
) -> Vec<String> {
    match selection {
        LayerSelection::Named(list) => list
            .iter()
            .filter(|name| reachable.contains(name))
            .map(|name| name.to_string())
            .collect(),
        LayerSelection::All => reachable.names().map(str::to_string).collect(),
    }
}

/// Whether one level of one layer is answered for.
pub(crate) fn level_is_selected(
    selection: LevelSelection<'_>,
    declared: &[mosaica_types::layer::LevelDeclaration],
    level: u32,
    zoom: u8,
) -> bool {
    // A layer with no declared levels sits entirely at level 0, so a level selection naming
    // other layers must not blank it: otherwise `levels: [1]` alongside `layers: "all"` would
    // serve nothing from a flat or treed layer in the same request.
    if declared.is_empty() {
        return true;
    }
    match selection {
        LevelSelection::All => true,
        LevelSelection::Named(levels) => levels.contains(&level),
        LevelSelection::Declared => {
            // No level declares a zoom range, so there is nothing to check against and every
            // level answers.
            if !declared.iter().any(|d| d.zoom.is_some()) {
                return true;
            }
            match declared.iter().find(|d| d.level == level) {
                Some(d) => match d.zoom {
                    Some((lo, hi)) => (lo..=hi).contains(&u32::from(zoom)),
                    // No range for this level: nothing to be outside of.
                    None => true,
                },
                // No declaration for this level: answers, rather than vanishing silently.
                None => true,
            }
        }
    }
}

/// The authored shapes an artifact's content carries at its layer's shape slot, taken out of the
/// content, whose slot is left blank. `None` where the layer authors no shape or the slot does not
/// read as one.
fn take_authored_shapes(
    declaration: &mosaica_types::layer::LayerDeclaration,
    content: &mut [String],
) -> Option<mosaica_lifecycle::membership::ArtifactShapes> {
    let (slot, _) = declaration.authored_shape()?;
    let text = content.get_mut(slot)?;
    let shapes = mosaica_lifecycle::membership::ArtifactShapes::from_content_text(text);
    text.clear();
    shapes
}

/// An artifact's supplied content as it may leave the engine: the values, with an authored
/// shape's slot blank, and the shapes taken from that slot.
pub(crate) struct Supplied {
    pub(crate) values: Vec<String>,
    pub(crate) authored: Option<mosaica_lifecycle::membership::ArtifactShapes>,
    /// Where the layer's authored shape sits among the values, where it authors one.
    shape_slot: Option<usize>,
}

impl Supplied {
    /// The first text content, which names the artifact: `None` where there is none, or where
    /// the first content is the authored shape's slot.
    pub(crate) fn first_text(&self) -> Option<&str> {
        if self.shape_slot == Some(0) {
            return None;
        }
        self.values.first().map(String::as_str)
    }

    /// The artifact's own name: its first text, where that is not empty.
    pub(crate) fn name(&self) -> Option<String> {
        self.first_text()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }
}

/// A drawn geometry as the wire carries it: parts, then rings, then vertices in grid units.
pub(crate) type Rings = Vec<Vec<Vec<[u32; 2]>>>;

/// One view's authored shape as rings for the wire; `None` where the artifact authored none for
/// `view` or its bytes do not decode.
pub(crate) fn authored_rings(
    shapes: &mosaica_lifecycle::membership::ArtifactShapes,
    view: &str,
    zoom: Option<u8>,
) -> Option<Rings> {
    let shape = mosaica_spatial::shape::Shape::decode(shapes.for_view(view)?).ok()?;
    Some(crate::shapes::served_rings(&shape, zoom))
}

/// How long a chain of dependencies one request will follow.
///
/// The dependency graph is acyclic by construction, so a real chain is one or two links deep.
/// This is a backstop against a disagreeing store: past it, a request refuses the chain rather
/// than risk unbounded recursion.
const DEPENDENCY_CHAIN_MAX: u32 = 16;

/// What [`Engine::gated_artifact`] answers: the artifact, located, with its level's row forms and
/// the verdict's two outputs.
pub(super) struct GatedArtifact {
    pub(super) name: String,
    pub(super) level: u32,
    pub(super) ordinal: u32,
    pub(super) entity: mosaica_types::EntityId,
    pub(super) layer: mosaica_types::layer::RegisteredLayer,
    /// The level in every side, with the masked counts the gate read, so derived geometry reuses
    /// this gate's walk of the mask instead of repeating it.
    pub(super) forms: crate::layer_read::LevelForms,
    pub(super) masked_count: u64,
    pub(super) position: Option<u32>,
}

/// One request's state, as the dependency prerequisite needs it: gathered once per response,
/// since every field is a property of the request rather than of the artifact under test.
///
/// The view is [`ServedView`] whole because a dependency's verdict must be reached with the same
/// generation, segments and deny mask as the artifact it is attached to — otherwise the two could
/// disagree about what the viewer sees.
pub(crate) struct DependencyContext<'a> {
    served: &'a ServedView<'a>,
    mask: &'a ShardedMask,
    reachable: &'a mosaica_lifecycle::ResolvedLayers,
    /// Each target layer's label test for this viewer, settled once per request: a named default
    /// is evaluated once rather than once per candidate.
    labels: std::cell::RefCell<rustc_hash::FxHashMap<String, crate::artifacts::LabelGate<'a>>>,
    /// The first error a dependency's verdict met. The verdict is a `bool` asked from inside
    /// another verdict, so the error waits here for [`Self::finish`].
    failed: std::cell::RefCell<Option<EngineError>>,
}

impl<'a> DependencyContext<'a> {
    /// `result`'s value, or `None` with its error held for [`Self::finish`]: for a read made where
    /// an error cannot be returned, inside a closure the request is answering from.
    pub(crate) fn held<T>(&self, result: Result<T>) -> Option<T> {
        result
            .map_err(|error| {
                self.failed.borrow_mut().get_or_insert(error);
            })
            .ok()
    }

    /// The first error any dependency's verdict met, which the request answers with in place of
    /// the verdicts reached since.
    pub(crate) fn finish(&self) -> Result<()> {
        match self.failed.borrow_mut().take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn new(
        served: &'a ServedView<'a>,
        mask: &'a ShardedMask,
        reachable: &'a mosaica_lifecycle::ResolvedLayers,
    ) -> Self {
        DependencyContext {
            served,
            mask,
            reachable,
            labels: Default::default(),
            failed: Default::default(),
        }
    }
}

/// What [`Engine::warm_artifact_projections`] did, for the open's own log line: a count and a
/// duration, naming no artifact and no principal.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WarmedProjections {
    /// How many `(view, layer, level)` forms were asked for.
    pub(crate) levels: u64,
    /// What the whole pass took, moved off the request path and onto open.
    pub(crate) elapsed_ms: u64,
}

impl Engine {
    /// Build every live level's row-space projection now, so no request pays to build one.
    /// Building one costs seconds at corpus scale (23.3s measured for rung 3's largest layer),
    /// and left lazy that cost lands on whichever request is first after a restart.
    ///
    /// Builds only structures shared by every viewer of a `(view, layer, level)` — never the
    /// masked counts, the gate or the verdicts, which need a session.
    ///
    /// A layer suppressed at open is skipped: its levels are then built on first use.
    pub(crate) fn warm_artifact_projections(&self) -> WarmedProjections {
        let started = std::time::Instant::now();
        let mut warmed = WarmedProjections::default();
        let generation = self.generation.load();
        let layers = self.write.live().registered_layers();
        if layers.is_empty() {
            return warmed;
        }
        let first = generation.shards().next().map(|shard| shard.id());
        for shard in generation.shards() {
            for (view, view_data) in &shard.data.views {
                let Ok(segments) = segments_with_row_bases(view, view_data) else {
                    continue;
                };
                for layer in &layers {
                    if !layer.declaration.draws_on(view) {
                        continue;
                    }
                    // Same order as `serve_artifacts`: a suppressed or deleted layer is served to
                    // nobody, so its form is not worth building.
                    if generation.layer_denied(layer.space) {
                        continue;
                    }
                    let vocabulary = predicate_vocabulary(&generation, &layer.declaration);
                    let code_of_key = |key: &str| match vocabulary {
                        Some(vocabulary) => vocabulary.code_of(key),
                        None => key.parse::<u32>().ok(),
                    };
                    for level in 0..layer.runs.len() as u32 {
                        // The engine's own pool: the decode fans out, and running it outside
                        // `install` would spill onto rayon's global pool instead of the one sized
                        // for this deployment.
                        let ((rows, level_version), lineage) = self.pool.install(|| {
                            self.write.live().with_artifacts(|written| {
                                let form = self.form_in(
                                    &generation,
                                    shard,
                                    view,
                                    view_data,
                                    &segments,
                                    layer,
                                    level,
                                    &code_of_key,
                                    written,
                                );
                                let lineage = crate::layer_read::lineage_at(
                                    written,
                                    &layer.declaration.name,
                                    level,
                                    shard.id(),
                                    form.1,
                                );
                                (form, lineage)
                            })
                        });
                        warmed.levels += 1;
                        // What follows is of the level's records, which every shard shares.
                        if Some(shard.id()) != first {
                            continue;
                        }
                        // Warmed for the same reason as the row form: derived from the level's
                        // records alone (~0.5s each at rung 3's largest), not from a mask or
                        // principal.
                        self.level_lineage(layer, level, lineage, &rows);
                        if let Some(runs) = layer.runs.get(level as usize) {
                            self.level_content(&generation, layer, level, level_version, runs);
                        }
                    }
                }
            }
        }
        warmed.elapsed_ms = started.elapsed().as_millis() as u64;
        warmed
    }

    /// One artifact, located and gated for one principal.
    ///
    /// `None` is the only failure shape: an identifier naming nothing, naming a point, naming an
    /// artifact this principal cannot reach or that is suppressed, on another view, or below its
    /// layer's existence criterion, are all one answer. A route that told these apart would let a
    /// viewer probe for what exists beyond what they may see.
    pub(super) fn gated_artifact(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        id: MosaicaId,
    ) -> Result<Option<GatedArtifact>> {
        let (session, generation, view) = (served.session, served.generation, served.name);
        let Some((space, entity)) = self.identity_key.invert_artifact(id) else {
            return Ok(None);
        };

        let Some((level, ordinal)) = self.write.live().locate_artifact(space, entity) else {
            return Ok(None);
        };
        let Some(layer) = self.write.live().layer_holding(space) else {
            return Ok(None);
        };
        let name = layer.declaration.name.clone();
        if !layer.declaration.draws_on(view) {
            return Ok(None);
        }
        // Another view's artifact is refused here, indistinguishable from one naming nothing.
        if !self.write.live().with_artifacts(|store| {
            store.drawn_in_view(&name, level, ordinal, crate::artifacts::view_key(view))
        }) {
            return Ok(None);
        }
        // Reachability, then live suppression — same order as `serve_artifacts`.
        let reachable = self.reachable_layers(served.session);
        if !reachable.contains(&name) || generation.layer_denied(layer.space) {
            return Ok(None);
        }

        // Resolved from the same generation the viewport uses: same membership, either route.
        let forms = self.counted_level(
            served,
            mask,
            &layer,
            level,
            crate::figures::Geometry::declared(&layer.declaration),
        )?;
        let ctx = DependencyContext::new(served, mask, &reachable);
        let dependency_served = self.dependency_gate(&ctx);
        let labels = self.label_gate(session, &layer.declaration);
        let verdict = forms
            .view(served, mask, &layer, labels, &dependency_served, true)
            .verdict(entity, ordinal);
        ctx.finish()?;
        let crate::artifacts::ArtifactVerdict::Serve {
            masked_count,
            position,
        } = verdict
        else {
            return Ok(None);
        };
        Ok(Some(GatedArtifact {
            name,
            level,
            ordinal,
            entity,
            layer,
            forms,
            masked_count,
            position,
        }))
    }

    /// Drill down on one artifact by the identifier a response handed out. Calls the same
    /// predicate the viewport does — [`crate::artifacts::ArtifactView::verdict`] — with no tile
    /// candidacy, since the caller already named the artifact.
    ///
    /// `None` is the only failure shape: nothing, a point rather than an artifact, an unreachable
    /// or suppressed layer, and an artifact below its existence criterion, are all one answer.
    /// That criterion tests the masked count, so it can only cross the bar as this viewer's own
    /// visible membership changes, never by a query about another artifact.
    ///
    /// The one expensive path — building a projection — is deployment-wide state keyed on what
    /// was published, not on who is asking, so its timing carries nothing about a principal.
    ///
    /// `zoom` is the depth the caller draws at; `None` serves the whole presimplified shape.
    pub fn artifact(
        &self,
        session: &Session,
        id: MosaicaId,
        view: &str,
        zoom: Option<u8>,
    ) -> Result<Option<ArtifactOut>> {
        let generation = self.generation.load_full();
        let mut probe = Probe::new();
        let open = self.open_view(session, &generation, view, &None, &mut probe)?;
        let (served, mask) = (&open.served, &open.mask);
        let mask_identity = served.mask_identity;
        let Some(gated) = self.gated_artifact(served, mask, id)? else {
            return Ok(None);
        };
        let GatedArtifact {
            name,
            level,
            ordinal,
            entity,
            layer,
            forms,
            masked_count,
            position,
        } = gated;
        // Same call as the viewport's, so the two routes cannot serve different content.
        let Some(supplied) = self.supplied_content(
            generation.space_content(layer.space).map(Arc::as_ref),
            &layer.declaration,
            level,
            ordinal,
            entity,
            position,
            true,
            // Reads the row directly: one artifact does not justify building the level's table.
            None,
        ) else {
            return Ok(None);
        };
        // Computed from the same composed mask the viewport uses, so the two routes cannot
        // disagree about one artifact's geometry.
        let declared_derived: Vec<crate::derived::ComputedProperty> = layer
            .declaration
            .content
            .computed
            .iter()
            .filter_map(|name| crate::derived::ComputedProperty::parse(name))
            .collect();
        // The same per-principal cache the viewport reads: a viewer moving the pointer back over
        // a cluster they already hovered pays nothing.
        let derived = if declared_derived.is_empty() {
            crate::derived::DerivedContent::default()
        } else {
            let key = crate::derived::cache::DerivedKey {
                token_id: mask_identity.token_id,
                view: view.to_string(),
                layer: name.clone(),
                level,
                ordinal,
                level_version: forms.level_version(),
                shards: mask_identity.shards,
                properties: crate::derived::cache::properties_bits(&declared_derived),
            };
            match forms.figures.as_ref().filter(|f| f.has_geometry()) {
                // The accumulation the level's own figures carry.
                Some(figures) => crate::derived::accumulated(&declared_derived, figures, ordinal),
                None => {
                    let content = self.derived_geometry.get_or_derive(key, || {
                        let locators = served.locators();
                        let visible = forms.visible(ordinal, mask, &locators);
                        crate::derived::compute(&declared_derived, &visible)
                    });
                    (*content).clone()
                }
            }
        };
        // The one shape a client draws; this route always answers for it.
        let mut derived = derived;
        self.drawn_shape(
            &layer.declaration,
            view,
            &name,
            level,
            ordinal,
            supplied.authored.as_ref(),
            &mut derived,
            zoom,
        );
        Ok(Some(ArtifactOut {
            content: supplied.values,
            layer: name.clone(),
            mosaica_id: id,
            key: self.write.live().with_artifacts(|store| {
                store.get(&name, level, ordinal).and_then(|r| r.key.clone())
            }),
            masked_count,
            derived,
            rung: level,
            // Always empty: naming a parent or target not in this response would disclose an
            // artifact this caller was never served.
            parent_ids: Vec::new(),
            target: None,
            // No filter or highlight on this route to answer about, and no viewport to scope it to.
            matched: None,
            highlighted: None,
            slot: None,
        }))
    }

    /// The predicate or authored kind of an artifact's one drawn geometry, filled into
    /// `derived.shape` beside the count.
    ///
    /// A predicate shape is the level's held canonical shape, served only for an artifact its
    /// own verdict already admitted.
    ///
    /// An authored shape is `authored`, taken from the supplied content's shape slot by
    /// [`Engine::supplied_content`], drawn for `view` alone. A slot that did not read as a shape
    /// draws nothing rather than a guess.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn drawn_shape(
        &self,
        declaration: &mosaica_types::layer::LayerDeclaration,
        view: &str,
        layer: &str,
        level: u32,
        ordinal: u32,
        authored: Option<&mosaica_lifecycle::membership::ArtifactShapes>,
        derived: &mut crate::derived::DerivedContent,
        zoom: Option<u8>,
    ) {
        match declaration.drawn_shape() {
            None | Some(crate::shapes::DrawnShape::Derived) => {}
            Some(crate::shapes::DrawnShape::Predicate) => {
                let held = match self.shapes.get(view, layer, level) {
                    Some(held) => held,
                    // Not yet held for this view: nothing warmed the level, so it is built here.
                    None => self.write.live().with_artifacts(|store| {
                        self.shapes.level(
                            view,
                            layer,
                            level,
                            store,
                            &crate::shapes::PersistedPieces::none(),
                        )
                    }),
                };
                if let Some(shape) = held.shapes.get(ordinal as usize).and_then(|s| s.as_ref()) {
                    derived.shape = Some(crate::shapes::served_rings(&shape.shape, zoom));
                }
            }
            Some(crate::shapes::DrawnShape::Authored) => {
                derived.shape = authored.and_then(|shapes| authored_rings(shapes, view, zoom));
            }
        }
    }

    /// The values of the content the predicate chose, or `None` where it cannot be read back.
    /// `position` is the verdict's: where that content has its values in the artifact's content
    /// row, which names one content for as long as the artifact exists.
    ///
    /// `Some(vec![])` is a layer declaring no supplied content; `None` is an artifact that
    /// should carry content and does not, which withholds the artifact. `materialise = false`
    /// copies nothing but must decide `Some`/`None` identically, since a projection must not
    /// move the row set.
    ///
    /// `table` is the level's contents, read once by the viewport pass, keeping a response off
    /// one zstd block read per artifact. `None` reads the one entity's row directly.
    ///
    /// Every route that serves or searches supplied content takes it from here. An authored
    /// shape's slot names every view of its layer and holds each one's canonical shape, so it is
    /// taken out of the values here and returned beside them, for drawing one view's shape.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn supplied_content(
        &self,
        content: Option<&mosaica_filter::RecordStack>,
        declaration: &mosaica_types::layer::LayerDeclaration,
        level: u32,
        ordinal: u32,
        entity: EntityId,
        position: Option<u32>,
        materialise: bool,
        table: Option<&crate::artifact_content::LevelContent>,
    ) -> Option<Supplied> {
        let mut values = self.supplied_values(
            content,
            &declaration.name,
            level,
            ordinal,
            entity,
            declaration.content.supplied.len(),
            position,
            materialise,
            table,
        )?;
        let authored = take_authored_shapes(declaration, &mut values);
        Some(Supplied {
            values,
            authored,
            shape_slot: declaration.authored_shape().map(|(slot, _)| slot),
        })
    }

    /// [`Engine::supplied_content`]'s values as the store holds them, the shape slot included.
    #[allow(clippy::too_many_arguments)]
    fn supplied_values(
        &self,
        content: Option<&mosaica_filter::RecordStack>,
        layer: &str,
        level: u32,
        ordinal: u32,
        entity: EntityId,
        kinds: usize,
        position: Option<u32>,
        materialise: bool,
        table: Option<&crate::artifact_content::LevelContent>,
    ) -> Option<Vec<String>> {
        let Some(position) = position else {
            return Some(Vec::new());
        };
        // The publication's own copy, the only home the content has before the manifest carries it.
        // A content withdrawn since the verdict's version of the record is not served, and no other
        // content is read in its place.
        let held = self.write.live().with_artifacts(|store| {
            let record = store.get(layer, level, ordinal)?;
            let set = record.contents.get(record.rank_at(position as usize)?)?;
            Some(match (&set.values, materialise) {
                (Some(values), true) => Some(values.clone()),
                (Some(_), false) => Some(Vec::new()),
                (None, _) => None,
            })
        });
        if let Some(values) = held? {
            return Some(values);
        }

        // Otherwise the layer space's record blob. Tags are `position × kinds + kind` against the
        // declaration.
        if kinds == 0 {
            return Some(Vec::new());
        }
        let base = (position as usize).checked_mul(kinds)?;
        let entity = u32::try_from(entity.raw()).ok()?;
        /// Every declared kind or none: a partial row looks, to a client, like content withheld.
        fn values_for<'a>(
            base: usize,
            kinds: usize,
            materialise: bool,
            text_at: impl Fn(u16) -> Option<&'a str>,
        ) -> Option<Vec<String>> {
            let mut values = Vec::with_capacity(if materialise { kinds } else { 0 });
            for k in 0..kinds {
                let text = text_at(u16::try_from(base + k).ok()?)?;
                if materialise {
                    values.push(text.to_string());
                }
            }
            Some(values)
        }
        // The two routes decide identically: a tag the table lacks is one the row lacked too.
        match table {
            Some(table) => {
                let tagged = table.tagged(entity)?;
                values_for(base, kinds, materialise, |tag| {
                    tagged
                        .binary_search_by_key(&tag, |(t, _)| *t)
                        .ok()
                        .map(|at| tagged[at].1.as_str())
                })
            }
            None => {
                let fields = content?.fields_of(entity).ok()??;
                values_for(base, kinds, materialise, |tag| {
                    fields
                        .iter()
                        .find(|f| f.tag == tag)
                        .and_then(|f| match &f.value {
                            mosaica_filter::RecordValue::Utf8(text) => Some(text.as_str()),
                            _ => None,
                        })
                })
            }
        }
    }

    /// Is the artifact this one attaches to served to this viewer? Calls the target's own
    /// `verdict`, not a cheaper summary — an approximation could serve a label for a cluster the
    /// viewer cannot see.
    ///
    /// Order matches the served layer's own path: reachability, then live disposition, then the
    /// level and slot, then the predicate — a cached reachability asked before a fresh
    /// disposition would let a suppression be outlived by a session.
    ///
    /// `depth` bounds the recursion against a store that disagrees with the declaration graph
    /// being acyclic.
    fn dependency_served(
        &self,
        ctx: &DependencyContext<'_>,
        attachment: &mosaica_lifecycle::membership::Attachment,
        depth: u32,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        // Unreachable and dropped-since-resolution are one answer: which applies is the fact withheld.
        if !ctx.reachable.contains(&attachment.layer) {
            return false;
        }
        let Some(layer) = self
            .write
            .live()
            .registered_layer(&attachment.layer)
            .filter(|layer| layer.space == attachment.space)
        else {
            return false;
        };
        if ctx.served.generation.layer_denied(layer.space) {
            return false;
        }
        // A layer not in this view has no membership here to serve.
        if !layer.declaration.draws_on(ctx.served.name) {
            return false;
        }
        let record = self.write.live().with_artifacts(|store| {
            store
                .get(&attachment.layer, attachment.level, attachment.ordinal)
                .map(|record| record.entity)
        });
        // The slot must answer with the entity the edge names. A hole is a deletion; a different
        // entity is an edge into an artifact that has been republished over. Both are absent.
        let Some(entity) = record.filter(|entity| *entity == attachment.entity) else {
            return false;
        };
        // The target's own membership and figures, as its serving route reads them; a
        // prerequisite never asks for geometry.
        let mut forms = self.level_forms(ctx.served, &layer, attachment.level);
        let counted = self.count_level(
            ctx.served,
            ctx.mask,
            layer.space,
            attachment.level,
            &mut forms,
            crate::figures::Geometry::None,
        );
        if ctx.held(counted).is_none() {
            return false;
        }
        let nested = |a: &mosaica_lifecycle::membership::Attachment| {
            self.dependency_served(ctx, a, depth - 1)
        };
        // A statement of its own, so the borrow ends before the verdict below follows a chain
        // back into this function.
        let labels = *ctx
            .labels
            .borrow_mut()
            .entry(attachment.layer.clone())
            .or_insert_with(|| self.label_gate(ctx.served.session, &layer.declaration));
        // Containment settled lazily and load-bearing: this runs once per candidate, so settling
        // a whole expression table here would turn a per-artifact question into
        // whole-population work.
        forms
            .view(ctx.served, ctx.mask, &layer, labels, &nested, true)
            .verdict(entity, attachment.ordinal)
            .is_served()
    }

    /// The prerequisite as the predicate takes it: a closure over one request's state.
    pub(crate) fn dependency_gate<'a>(
        &'a self,
        ctx: &'a DependencyContext<'a>,
    ) -> impl Fn(&mosaica_lifecycle::membership::Attachment) -> bool + 'a {
        move |attachment| self.dependency_served(ctx, attachment, DEPENDENCY_CHAIN_MAX)
    }

    /// The artifacts of `names` over every tile of `tiling`, walked whole: every one this
    /// principal reaches, that has a visible member inside the tiles, that passes the predicate
    /// and that the cut keeps, settled against itself and against `outside`.
    ///
    /// Four narrowings in this order, and the order is the disclosure control: reachability
    /// first, so a name the principal cannot reach never has its membership touched; candidacy
    /// second, keeping the count off artifacts outside the tiles; the predicate last.
    ///
    /// The count is over the whole membership, not the tiles: a per-viewport count would move as
    /// the viewer pans and let them difference two boxes for the members in between.
    ///
    /// The view arrives whole and is not resolved a second time, so an artifact and a point of
    /// one response are answered over the same row space.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn serve_artifacts(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        tiling: &Tiling,
        ask: &ArtifactAsk<'_>,
        names: Vec<String>,
        in_request: &std::collections::BTreeSet<String>,
        dependency_served: &dyn Fn(&mosaica_lifecycle::membership::Attachment) -> bool,
    ) -> Result<Settled> {
        if names.is_empty() {
            return Ok(Settled::default());
        }
        // The rows this request's tiles span in each side, which every set below is taken over.
        // Where they span none, no layer can have a candidate.
        let tile_rows = tile_rows(served, mask, tiling);
        if tile_rows.is_empty() {
            return Ok(Settled::default());
        }
        let sets = viewport_sets(served, &tile_rows, mask);
        let walked = self.walk_layers(served, ask, mask, names, &sets, dependency_served)?;
        Ok(settle_response(walked, in_request, &Outside::default()))
    }

    /// One pass over the requested layers and the levels this request selects of each: the gate,
    /// the masked count, the cut, and the row every surviving artifact is served as — with where
    /// it sits and what it points at recorded for the reconciliation after the walk.
    fn walk_layers(
        &self,
        served: &ServedView<'_>,
        ask: &ArtifactAsk<'_>,
        mask: &ShardedMask,
        names: Vec<String>,
        sets: &[ViewportSets<'_>],
        dependency_served: &dyn Fn(&mosaica_lifecycle::membership::Attachment) -> bool,
    ) -> Result<Walked> {
        let pass = ArtifactPass::new(served, ask, mask, dependency_served);

        let mut walked = Walked::default();
        for name in names {
            let Some(layer) = self.layer_pass(&pass, name)? else {
                continue;
            };
            if lineage_kind(layer.registered.declaration.hierarchy.kind).is_some() {
                walked.treed.insert(layer.name.clone());
            }
            let mut served_levels: Vec<ServedLevel> = Vec::new();
            for (number, runs) in layer.registered.runs.iter().enumerate() {
                let number = number as u32;
                // Skipped before the projection is built: costs nothing here, which matters for
                // a whole-layer response over a multi-level hierarchy.
                if !level_is_selected(
                    ask.levels,
                    &layer.registered.declaration.levels,
                    number,
                    ask.zoom,
                ) {
                    continue;
                }
                let level = self.level_pass(&layer, number, runs)?;
                let DrawnCut {
                    lineage,
                    passing,
                    served: cut,
                } = self.level_cut(&level, sets);
                served_levels.push(ServedLevel {
                    level: number,
                    rows: level.forms.rows.clone(),
                    lineage: Arc::clone(&lineage),
                    // Filled once the response's membership is settled, below.
                    served: std::collections::HashMap::new(),
                });
                // Built after candidacy: the filter decides nothing about which artifacts are
                // served.
                let matched = matched_sides(&level.forms, sets, |set| set.matched_here.as_ref());
                let highlighted =
                    matched_sides(&level.forms, sets, |set| set.highlighted_here.as_ref());
                let bits = |ordinal: u32| -> FilterBits {
                    (
                        matches_any(&level.forms, matched.as_deref(), ordinal),
                        matches_any(&level.forms, highlighted.as_deref(), ordinal),
                    )
                };
                self.assemble_level(&level, passing, &cut, &bits, &mut walked)?;
            }
            walked.served_layers.push(ServedLayer {
                name: layer.name,
                levels: served_levels,
            });
        }
        Ok(walked)
    }

    /// One requested layer, resolved for this request: the registration this walk reads it
    /// through, what is parsed once for it, and the three dispositions that drop it whole.
    pub(super) fn layer_pass<'a>(
        &self,
        pass: &'a ArtifactPass<'a>,
        name: String,
    ) -> Result<Option<LayerPass<'a>>> {
        let Some(registered) = self.write.live().registered_layer(&name) else {
            // Dropped between the resolution and here — the same answer as a gate failure.
            return Ok(None);
        };
        // A layer not declaring this view has no membership here to project.
        if !registered
            .declaration
            .views
            .iter()
            .any(|s| s == pass.served.name)
        {
            return Ok(None);
        }
        // Asked per request: reachability may be cached but a suppression verdict may not.
        if pass.served.generation.layer_denied(registered.space) {
            return Ok(None);
        }

        // Parsed once per layer, an unparseable name dropped rather than erroring the response,
        // and narrowed by the request so a property not asked for is never computed.
        let declared_derived: Vec<crate::derived::ComputedProperty> = registered
            .declaration
            .content
            .computed
            .iter()
            .filter_map(|name| crate::derived::ComputedProperty::parse(name))
            .filter(|property| pass.ask.computed.selects(*property))
            .collect();

        Ok(Some(LayerPass {
            pass,
            name,
            registered,
            declared_derived,
        }))
    }

    /// **Stage one: this level, resolved for this request** — the row form in every side and the
    /// version each is of, and the level's figures. Everything the gate, the cut and the
    /// assembly read of the level is settled here and read from there.
    pub(super) fn level_pass<'a>(
        &self,
        layer: &'a LayerPass<'a>,
        level: u32,
        runs: &'a mosaica_types::layer::ReservedRuns,
    ) -> Result<LevelPass<'a>> {
        let (pass, served) = (layer.pass, layer.pass.served);
        let forms = self.counted_level(
            served,
            pass.mask,
            &layer.registered,
            level,
            crate::figures::Geometry::declared(&layer.registered.declaration),
        )?;
        // The level's slots, where the request named a palette size.
        let slots = match pass.ask.palette {
            Some(palette) => self.cluster_slots(
                served,
                pass.mask,
                &layer.registered,
                level,
                palette,
                pass.dependency_served,
            )?,
            None => None,
        };
        Ok(LevelPass {
            layer,
            level,
            runs,
            forms,
            slots,
        })
    }

    /// The verdict over one level for this request's viewer, settled once and asked per
    /// candidate.
    pub(super) fn level_view<'a>(
        &self,
        level: &'a LevelPass<'a>,
    ) -> crate::artifacts::ArtifactView<'a, crate::compose::EffectiveMask> {
        let (layer, pass) = (level.layer, level.layer.pass);
        let served = pass.served;
        level.forms.view(
            served,
            pass.mask,
            &layer.registered,
            self.label_gate(served.session, &layer.registered.declaration),
            pass.dependency_served,
            false,
        )
    }

    /// What a treed layer draws at `zoom` over `bbox` under `budget`, as the treed frame of
    /// `POST /v1/artifacts/viewport` draws it for the same view, zoom, bbox and budget: the cut
    /// through the layer's artifacts this viewer is served with a visible member in the tiles.
    /// `None` where the layer is not one this viewer reads in this view, or not treed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn drawn_cut(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        zoom: u8,
        bbox: [f64; 4],
        budget: Option<u32>,
        layer: &str,
        dependency_served: &dyn Fn(&mosaica_lifecycle::membership::Attachment) -> bool,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<DrawnCut>> {
        let tiles_req = ViewportRequest::new(served.name, zoom, bbox, 0).cancel(cancel.clone());
        let tiling = self.tiling(served, &tiles_req, &mut Probe::new())?;
        let ask = ArtifactAsk {
            zoom,
            levels: LevelSelection::All,
            computed: ComputedSelection::Named(&[]),
            budget,
            rows: ArtifactRows::Full,
            cancel: cancel.clone(),
            palette: None,
        };
        let pass = ArtifactPass::new(served, &ask, mask, dependency_served);
        let Some(layer) = self.layer_pass(&pass, layer.to_string())? else {
            return Ok(None);
        };
        let Some(runs) = layer.registered.runs.first() else {
            return Ok(None);
        };
        if lineage_kind(layer.registered.declaration.hierarchy.kind).is_none() {
            return Ok(None);
        }
        let level = self.level_pass(&layer, 0, runs)?;
        let rows = tile_rows(served, mask, &tiling);
        Ok(Some(self.level_cut(&level, &viewport_sets(served, &rows, mask))))
    }

    /// The verdicts over the candidates `sets` proposes on one level, and the cut through those
    /// that pass.
    fn level_cut(&self, level: &LevelPass<'_>, sets: &[ViewportSets<'_>]) -> DrawnCut {
        let passing = self.gate_candidates(level, sets);
        let (lineage, served) = self.cut_level(level, &passing);
        DrawnCut {
            lineage,
            passing,
            served,
        }
    }

    /// **Stage two: the verdict, for every candidate the viewport touches** — the ordinals that
    /// pass, ascending, with the two outputs their verdict carried.
    fn gate_candidates(&self, level: &LevelPass<'_>, sets: &[ViewportSets<'_>]) -> Vec<Passing> {
        let view = self.level_view(level);
        // Every candidate is tested before any is cut: a loop that decided and pruned in one
        // step could let a node's neighbours reach its verdict.
        let present = present_in(&level.forms, sets, |ordinal| view.admits_label(ordinal));
        let mut passing = Vec::new();
        for ordinal in present.iter() {
            let Some(entity) = level.runs.entity_of(ordinal as u64).map(EntityId::new) else {
                continue;
            };
            let crate::artifacts::ArtifactVerdict::Serve {
                masked_count,
                position,
            } = view.verdict(entity, ordinal)
            else {
                continue;
            };
            passing.push((ordinal, entity, masked_count, position));
        }
        passing
    }

    /// **Stage three: the level's lineage and the cut through it** — which of the passing ordinals
    /// the budget leaves, ascending, and the lineage the membership column reads the level back
    /// through.
    fn cut_level(
        &self,
        level: &LevelPass<'_>,
        passing: &[Passing],
    ) -> (Arc<crate::cut::Lineage>, Vec<u32>) {
        let layer = level.layer;
        let lineage = self.level_lineage(
            &layer.registered,
            level.level,
            level.forms.lineage,
            level.forms.first(),
        );
        let ordinals: Vec<u32> = passing.iter().map(|&(o, ..)| o).collect();
        // Ascending and deduplicated, so the assembly's membership test is a binary search.
        // `prune_children` is a rendering choice, not a disclosure one: every artifact in either
        // set cleared its own criterion.
        let served = crate::cut::cut(
            &lineage,
            &ordinals,
            layer.pass.ask.budget,
            layer.registered.declaration.hierarchy.prune_children,
        );
        (lineage, served)
    }

    /// One level's lineage at `at`, which describes `rows`' records, held per generation:
    /// rebuilding it per request would cost ~96ms at ten million against ~3ms for the cut.
    ///
    /// Read from every artifact's parent list, not only the passing ones, since one lineage
    /// serves every viewer; a cut is taken over the passing nodes alone, so a withheld ancestor
    /// never appears in a viewer's tree. Within-level edges only: a tiered layer's cross-level
    /// edges are containment information, not a ladder to coarsen along, so such a layer's
    /// lineage is empty here.
    pub(crate) fn level_lineage(
        &self,
        layer: &mosaica_types::layer::RegisteredLayer,
        number: u32,
        at: crate::cut::LineageAt,
        rows: &crate::artifacts::ArtifactRows,
    ) -> Arc<crate::cut::Lineage> {
        self.lineages
            .get_or_build(&layer.declaration.name, number, at, || {
                let records = rows.records();
                let edges = (0..records.len() as u32).map(|ordinal| {
                    let within = records
                        .parents(ordinal)
                        .iter()
                        .filter(move |parent| parent.level == number)
                        .map(|parent| parent.ordinal);
                    (ordinal, within)
                });
                match lineage_kind(layer.declaration.hierarchy.kind) {
                    Some(true) => crate::cut::Lineage::dag(edges),
                    _ => crate::cut::Lineage::new(edges),
                }
            })
    }

    /// **Stage four: the row every survivor is served as** — its content, its derived shape, its
    /// two edges recorded for the reconciliation after the walk, and the placement they resolve
    /// against.
    ///
    /// A level is served in key order, then its keyless artifacts in ordinal order, so a level a
    /// build published and one published at a running service are served alike.
    fn assemble_level(
        &self,
        level: &LevelPass<'_>,
        passing: Vec<Passing>,
        cut: &[u32],
        bits: &dyn Fn(u32) -> FilterBits,
        walked: &mut Walked,
    ) -> Result<()> {
        let survivors: Vec<Passing> = passing
            .into_iter()
            .filter(|&(ordinal, ..)| cut.binary_search(&ordinal).is_ok())
            .collect();
        let contents = self.level_contents_of(level, !survivors.is_empty());
        let mut keyed: Vec<(Option<String>, Passing)> = self
            .keys_of(level, survivors.iter().map(|at| at.0))
            .into_iter()
            .zip(survivors)
            .collect();
        keyed.sort_unstable_by(|(a, at), (b, bt)| {
            (a.is_none(), a, at.0).cmp(&(b.is_none(), b, bt.0))
        });
        for (key, at) in keyed {
            let content = match self.supplied_content(
                level
                    .layer
                    .pass
                    .served
                    .generation
                    .space_content(level.layer.registered.space)
                    .map(Arc::as_ref),
                &level.layer.registered.declaration,
                level.level,
                at.0,
                at.1,
                at.3,
                level.layer.pass.ask.rows == ArtifactRows::Full,
                contents.as_deref(),
            ) {
                Some(supplied) => supplied.values,
                // Content restored from a packed extent carries no values yet, and is withheld
                // rather than served with its description missing. Asked under the identity
                // projection too: an identity response must not carry a row the full response
                // would withhold.
                None => continue,
            };
            self.assemble_one(level, content, key, at, bits(at.0), walked)?;
        }
        Ok(())
    }

    /// [`Engine::level_content`] for a level pass, `None` where nothing is `wanted`.
    pub(super) fn level_contents_of(
        &self,
        level: &LevelPass<'_>,
        wanted: bool,
    ) -> Option<Arc<crate::artifact_content::LevelContent>> {
        let layer = level.layer;
        if !wanted {
            return None;
        }
        self.level_content(
            layer.pass.served.generation,
            &layer.registered,
            level.level,
            level.forms.level_version(),
            level.runs,
        )
    }

    /// The publisher's key of each of `ordinals` at this level, read from the level's slots under
    /// one look at the store, and `None` where the ask carries no payload.
    pub(super) fn keys_of(
        &self,
        level: &LevelPass<'_>,
        ordinals: impl Iterator<Item = u32>,
    ) -> Vec<Option<String>> {
        let full = level.layer.pass.ask.rows == ArtifactRows::Full;
        self.write.live().with_artifacts(|store| {
            let slots = store.slots(&level.layer.name, level.level);
            ordinals
                .map(|ordinal| {
                    slots
                        .get(ordinal as usize)
                        .and_then(Option::as_ref)
                        .and_then(|record| record.key.clone())
                        .filter(|_| full)
                })
                .collect()
        })
    }

    /// One artifact the gate admitted, as it is served: `content`, its identifier, its derived
    /// geometry and its two filter bits, with where it sits and what it points at recorded in
    /// `walked` for the reconciliation. Absent where its identifier cannot be formed.
    pub(super) fn assemble_one(
        &self,
        level: &LevelPass<'_>,
        content: Vec<String>,
        key: Option<String>,
        (ordinal, entity, masked_count, _): Passing,
        (matched, highlighted): FilterBits,
        walked: &mut Walked,
    ) -> Result<()> {
        let (layer, pass) = (level.layer, level.layer.pass);
        let (served, ask) = (pass.served, pass.ask);
        let (name, number) = (&layer.name, level.level);
        let rows: &crate::artifacts::ArtifactRows = level.forms.first();
        let full = ask.rows == ArtifactRows::Full;
        // Checked once per artifact served: without this a client that has gone is discovered
        // only once the whole frame is ready, after minutes deriving geometry nobody reads.
        check_cancelled(&ask.cancel)?;
        // The blinding is total over the allocator's space; a failure means the manifest and
        // allocator disagree, and dropping the artifact is the fail-closed reading of that.
        let Some(mosaica_id) = self.artifact_id(layer.registered.space, entity) else {
            return Ok(());
        };
        // From the composed mask, and only from it: every property below is a function of the
        // artifact's membership intersected with what this principal may see. Skipped under the
        // identity projection, whose point is that the derived sweep decides nothing about which
        // rows are served. Held per principal between requests: the cache key names the
        // principal so a hit never answers for a different one.
        let derived = if !full || layer.declared_derived.is_empty() {
            crate::derived::DerivedContent::default()
        } else {
            let key = crate::derived::cache::DerivedKey {
                token_id: served.mask_identity.token_id,
                view: served.name.to_string(),
                layer: name.clone(),
                level: number,
                ordinal,
                level_version: level.forms.level_version(),
                shards: served.mask_identity.shards,
                properties: crate::derived::cache::properties_bits(&layer.declared_derived),
            };
            // The accumulation where the level has one; nothing here walks the mask again.
            match level.forms.figures.as_ref().filter(|f| f.has_geometry()) {
                Some(figures) => {
                    crate::derived::accumulated(&layer.declared_derived, figures, ordinal)
                }
                None => (*self.derived_geometry.get_or_derive(key, || {
                    let visible = level.forms.visible(ordinal, pass.mask, &pass.locators);
                    crate::derived::compute(&layer.declared_derived, &visible)
                }))
                .clone(),
            }
        };
        let parents: Vec<(String, u32, u32)> = rows
            .parents(ordinal)
            .iter()
            .map(|p| (name.clone(), p.level, p.ordinal))
            .collect();
        // Recorded, not resolved: a parent or attachment target may sit in a level this loop has
        // not reached yet.
        walked
            .served_at
            .insert((name.clone(), number, ordinal), mosaica_id);
        walked.placed.push(Placement {
            at: (name.clone(), number, ordinal),
            parents,
            attached_to: rows
                .attachment(ordinal)
                .map(|a| (a.layer.clone(), a.level, a.ordinal)),
        });
        walked.out.push(ArtifactOut {
            content,
            layer: name.clone(),
            mosaica_id,
            key,
            masked_count,
            derived,
            // On a treed layer this is recomputed below as the response-local chain depth.
            rung: number,
            // Both filled in below, once the response's own membership is settled.
            parent_ids: Vec::new(),
            target: None,
            matched,
            highlighted,
            slot: level.slots.as_ref().and_then(|slots| slots.get(ordinal)),
        });
        Ok(())
    }
}

/// One artifact the gate admitted: its ordinal, its entity, the masked count its verdict carried
/// and the position of the content that verdict chose.
pub(crate) type Passing = (u32, EntityId, u64, Option<u32>);

/// One treed level's cut for one request: the level's lineage, the artifacts whose verdict passed
/// among the candidates, and the ordinals of those the cut draws, ascending.
pub(crate) struct DrawnCut {
    pub(crate) lineage: Arc<crate::cut::Lineage>,
    pub(crate) passing: Vec<Passing>,
    pub(crate) served: Vec<u32>,
}

impl DrawnCut {
    /// The passing artifacts among `ordinals`, which is ascending.
    pub(crate) fn passing_in(&self, ordinals: &[u32]) -> Vec<Passing> {
        self.passing
            .iter()
            .copied()
            .filter(|(ordinal, ..)| ordinals.binary_search(ordinal).is_ok())
            .collect()
    }
}

/// What a walk of a layer's artifacts is asked for, apart from where: the depth drawn at, the
/// levels, the computed properties, the budget a treed layer is cut to, the columns and the
/// request's cancellation.
pub(crate) struct ArtifactAsk<'a> {
    pub(crate) zoom: u8,
    pub(crate) levels: LevelSelection<'a>,
    pub(crate) computed: ComputedSelection<'a>,
    pub(crate) budget: Option<u32>,
    pub(crate) rows: ArtifactRows,
    pub(crate) cancel: Option<CancelToken>,
    /// The palette size each served artifact's slot is chosen for, where one was named.
    pub(crate) palette: Option<u8>,
}

/// **The artifacts pass of one request**: what every layer and every level of it is answered
/// against, resolved once before the walk.
pub(super) struct ArtifactPass<'a> {
    pub(super) served: &'a ServedView<'a>,
    pub(super) ask: &'a ArtifactAsk<'a>,
    /// The viewer's composed mask, which every verdict and every derived property reads.
    pub(super) mask: &'a ShardedMask,
    /// The dependency prerequisite, closed over this request's state.
    pub(super) dependency_served: &'a dyn Fn(&mosaica_lifecycle::membership::Attachment) -> bool,
    /// Each side's locator, in the order of the view's sides.
    pub(super) locators: Vec<crate::derived::RowLocator<'a>>,
}

impl<'a> ArtifactPass<'a> {
    pub(super) fn new(
        served: &'a ServedView<'a>,
        ask: &'a ArtifactAsk<'a>,
        mask: &'a ShardedMask,
        dependency_served: &'a dyn Fn(&mosaica_lifecycle::membership::Attachment) -> bool,
    ) -> Self {
        ArtifactPass {
            served,
            ask,
            mask,
            dependency_served,
            locators: served.locators(),
        }
    }
}

/// One layer of that pass: the registration this walk reads it through, and what is parsed once
/// for it.
pub(super) struct LayerPass<'a> {
    pub(super) pass: &'a ArtifactPass<'a>,
    pub(super) name: String,
    pub(super) registered: mosaica_types::layer::RegisteredLayer,
    /// The derived properties this layer declares **and** this request asked for.
    declared_derived: Vec<crate::derived::ComputedProperty>,
}

/// One level of one layer, as [`Engine::level_pass`] settles it: the values the gate, the cut and
/// the assembly all read, so that none of them resolves one of its own.
pub(super) struct LevelPass<'a> {
    pub(super) layer: &'a LayerPass<'a>,
    pub(super) level: u32,
    /// The level's reserved entity runs — ordinal to entity.
    pub(super) runs: &'a mosaica_types::layer::ReservedRuns,
    /// The level's row form in every side and the version each is of, with its masked counts and
    /// accumulated geometry where it has them — none on an artifact-major level.
    pub(super) forms: crate::layer_read::LevelForms,
    /// This viewer's slots over the level, where the request named a palette size.
    pub(super) slots: Option<Arc<crate::slots::LevelSlots>>,
}

/// The row-space sets one response, or one tile of it, is answered over in one side, composed
/// once for every layer in it.
pub(super) struct ViewportSets<'a> {
    /// Index into `served.sides`.
    pub(super) place: usize,
    /// The viewer's leaf in the side.
    pub(super) mask: &'a crate::compose::EffectiveMask,
    /// The tiles as one masked set — the candidate generator every level walks.
    pub(super) viewport: crate::tile_index::Viewport<'a>,
    pub(super) matched_here: Option<croaring::Bitmap>,
    pub(super) highlighted_here: Option<croaring::Bitmap>,
}

/// The rows the request's tiles span in each side the request reads: the spans
/// [`Tiling::domain_in`] gives the side's filter, so the two cannot disagree about which rows a
/// request covers. A side where the tiles span no row is not here.
pub(super) fn tile_rows(
    served: &ServedView<'_>,
    mask: &ShardedMask,
    tiling: &Tiling,
) -> Vec<(usize, croaring::Bitmap)> {
    served
        .filtered_sides(mask)
        .map(|(place, ..)| (place, spans_rows(&tiling.domain_in(place).0)))
        .filter(|(_, rows)| !rows.is_empty())
        .collect()
}

/// `spans` as one set of rows.
pub(super) fn spans_rows(spans: &[Range<u32>]) -> croaring::Bitmap {
    let mut rows = croaring::Bitmap::new();
    for span in spans {
        rows.add_range(span.clone());
    }
    rows
}

/// The three sets every layer is answered against in each side of `rows`, composed once from the
/// rows the tiles span there.
pub(super) fn viewport_sets<'a>(
    served: &ServedView<'_>,
    rows: &'a [(usize, croaring::Bitmap)],
    mask: &'a ShardedMask,
) -> Vec<ViewportSets<'a>> {
    rows.iter()
        .filter_map(|(place, rows)| {
            let mask = served.leaf(mask, *place)?;
            // Hoisted out of every layer and every artifact, built once per request.
            let viewport = crate::tile_index::Viewport::compose(rows, mask);
            Some(ViewportSets {
                place: *place,
                mask,
                // `None` is an unfiltered request — no column on the wire.
                matched_here: mask.matched_rows(viewport.here()),
                // `None` is a request carrying no highlight.
                highlighted_here: mask.highlighted_rows(viewport.here()),
                viewport,
            })
        })
        .collect()
}

/// The artifacts of one level holding a member the viewer sees in `sets`, ascending: each side
/// proposes candidates from its own form and probes each against its own leaf, and an artifact is
/// present where any side finds one. An artifact `admits_label` withholds has no membership
/// probed.
pub(super) fn present_in(
    forms: &crate::layer_read::LevelForms,
    sets: &[ViewportSets<'_>],
    admits_label: impl Fn(u32) -> bool,
) -> croaring::Bitmap {
    let mut present = croaring::Bitmap::new();
    for set in sets {
        let rows: &crate::artifacts::ArtifactRows = &forms.rows[set.place];
        // Replaces a sweep over every ordinal: an artifact in no node the viewport touches has no
        // visible member, so skipping it withholds nothing. Holes and artifacts with no projected
        // membership stay live on the identifier route, which walks no index. Or, on a row-major
        // level, a scan over the viewport intersected with the mask.
        let candidates = rows.candidacy(&set.viewport, forms.side_figures[set.place].as_deref());
        for ordinal in candidates.iter() {
            if present.contains(ordinal) || !admits_label(ordinal) {
                continue;
            }
            // Every candidate pays a masked probe, on whichever route is cheapest for it.
            if rows.candidate_in(ordinal, &candidates, &set.viewport, set.mask) {
                present.add(ordinal);
            }
        }
    }
    present
}

/// What a request's filter, or its highlight, admits of one level in `sets`: in each side,
/// [`crate::artifacts::ArtifactRows::matched`] over the rows `admitted` gives there, or `None`
/// where the request carries no such expression.
pub(super) fn matched_sides<'s>(
    forms: &crate::layer_read::LevelForms,
    sets: &'s [ViewportSets<'_>],
    admitted: impl Fn(&'s ViewportSets<'_>) -> Option<&'s croaring::Bitmap>,
) -> Option<Vec<(usize, crate::artifacts::Matched<'s>)>> {
    let sides: Vec<_> = sets
        .iter()
        .filter_map(|set| Some((set.place, forms.rows[set.place].matched(admitted(set)?))))
        .collect();
    (!sides.is_empty()).then_some(sides)
}

/// Whether the artifact at `ordinal` holds a member [`matched_sides`] admits in any side.
pub(super) fn matches_any(
    forms: &crate::layer_read::LevelForms,
    matched: Option<&[(usize, crate::artifacts::Matched<'_>)]>,
    ordinal: u32,
) -> Option<bool> {
    matched.map(|sides| {
        sides
            .iter()
            .any(|(place, matched)| forms.rows[*place].matches(matched, ordinal))
    })
}

impl Walked {
    /// Whether this walk holds the artifact at `(layer, level, ordinal)`.
    pub(super) fn serves(&self, at: &(String, u32, u32)) -> bool {
        self.served_at.contains_key(at)
    }
}

/// What one walk of the requested layers produced, before the response's own membership settles
/// which of it is served. Held outside the level it is written from, so the assembly can write
/// here while the level's row form, counts and contents are still borrowed.
#[derive(Default)]
pub(super) struct Walked {
    out: Vec<ArtifactOut>,
    /// The treed layers, whose rung is the response-local chain depth, applied after the row
    /// set is final.
    treed: std::collections::BTreeSet<String>,
    served_at: std::collections::BTreeMap<(String, u32, u32), MosaicaId>,
    /// Positionally aligned with `out`: where each artifact sits, and what it points at.
    placed: Vec<Placement>,
    served_layers: Vec<ServedLayer>,
}

/// Artifacts served in another frame of the same response, by `(layer, level, ordinal)`: their
/// identifiers and their two filter bits, which a dependent in this frame may name and take.
pub(super) type Outside = std::collections::BTreeMap<(String, u32, u32), (MosaicaId, FilterBits)>;

/// One walk, reconciled: the rows served, each walked layer's served set for the membership
/// column, and every row served by its address.
#[derive(Default)]
pub(super) struct Settled {
    pub(super) out: Vec<ArtifactOut>,
    pub(super) served_layers: Vec<ServedLayer>,
    pub(super) served_at: Outside,
}

/// Reconcile the walk against itself: drop the dependents whose target this response does not
/// hold, resolve the two edges that may only name rows the response carries, and rank the treed
/// layers over the forest that is left. A target in `outside` is held by the response, in another
/// of its frames.
pub(super) fn settle_response(
    walked: Walked,
    in_request: &std::collections::BTreeSet<String>,
    outside: &Outside,
) -> Settled {
    let Walked {
        out,
        treed,
        mut served_at,
        placed,
        mut served_layers,
    } = walked;
    // The cut ran after the verdicts, so a dependent may have passed on a target this response
    // then removed. Dropped here, before parents are resolved, so its own name leaves
    // `served_at` too — one response must never describe a cluster it does not contain.
    let dropped = orphaned_dependents(&placed, in_request, &mut served_at, outside);

    // A dependent's masked count is its own; its two filter bits are copied from the target
    // instead, since a label's own membership is often empty and would read `false` for every
    // filter — safe since the drop above guarantees the target's row is present.
    let bits_at: std::collections::BTreeMap<&(String, u32, u32), FilterBits> = placed
        .iter()
        .zip(&out)
        .map(|(place, artifact)| (&place.at, (artifact.matched, artifact.highlighted)))
        .collect();
    let target_bits: Vec<Option<FilterBits>> = placed
        .iter()
        .map(|place| {
            place
                .attached_to
                .as_ref()
                .filter(|target| in_request.contains(&target.0))
                .and_then(|target| {
                    bits_at
                        .get(target)
                        .copied()
                        .or_else(|| outside.get(target).map(|&(_, bits)| bits))
                })
        })
        .collect();

    // A parent, and a target, are named only where also in this response — the whole disclosure
    // rule for both fields. One withheld — below its own criterion, suppressed, or dropped by
    // the budget — carries no entry, indistinguishable from a root or an unattached row; naming
    // it would tell the viewer a coarser artifact exists that they are not cleared to see.
    let mut served = Vec::with_capacity(out.len());
    let mut kept = Vec::with_capacity(out.len());
    for (((mut artifact, place), dropped), target_bit) in
        out.into_iter().zip(&placed).zip(dropped).zip(target_bits)
    {
        if dropped {
            continue;
        }
        artifact.target = place.attached_to.as_ref().and_then(|target| {
            served_at
                .get(target)
                .copied()
                .or_else(|| outside.get(target).map(|&(id, _)| id))
        });
        if let Some((matched, highlighted)) = target_bit {
            artifact.matched = matched;
            artifact.highlighted = highlighted;
        }
        artifact.parent_ids = place
            .parents
            .iter()
            .filter_map(|key| served_at.get(key))
            .copied()
            .collect();
        artifact.parent_ids.sort_unstable_by_key(|id| id.raw());
        artifact.parent_ids.dedup();
        kept.push(place.at.clone());
        served.push(artifact);
    }
    // A treed layer's rung is the response-local depth — the longest parent chain in the forest
    // this response's own `parent_ids` form. Computed after the cut, content withholds and the
    // dependent drop, so a row whose ancestors were pruned or withheld is a root and reads 0.
    //
    // Settled before the membership column's served set is handed over: the column ranks by
    // this rung, never the stored depth, which counts withheld nodes a viewer cannot see.
    let parents_of: std::collections::HashMap<u64, Vec<u64>> = served
        .iter()
        .filter(|a| treed.contains(&a.layer))
        .map(|a| {
            (
                a.mosaica_id.raw(),
                a.parent_ids.iter().map(|p| p.raw()).collect(),
            )
        })
        .collect();
    let rungs = response_rungs(&parents_of);
    for artifact in served.iter_mut().filter(|a| treed.contains(&a.layer)) {
        artifact.rung = rungs.get(&artifact.mosaica_id.raw()).copied().unwrap_or(0);
    }
    // The membership column's served set is `served_at` after the drop: exactly the artifacts
    // in `served`, each with its response-local rung.
    for ((name, level, ordinal), mosaica_id) in &served_at {
        if let Some(slot) = served_layers
            .iter_mut()
            .find(|l| &l.name == name)
            .and_then(|l| l.levels.iter_mut().find(|l| l.level == *level))
        {
            let rung = rungs.get(&mosaica_id.raw()).copied().unwrap_or(0);
            slot.served.insert(*ordinal, (*mosaica_id, rung));
        }
    }
    let served_at = kept
        .into_iter()
        .zip(&served)
        .map(|(at, artifact)| {
            (
                at,
                (
                    artifact.mosaica_id,
                    (artifact.matched, artifact.highlighted),
                ),
            )
        })
        .collect();
    Settled {
        out: served,
        served_layers,
        served_at,
    }
}

/// Whether a layer's kind holds a lineage the cut climbs and the rung is counted over —
/// `Some(dag)` for roll-up kinds, `None` for flat and levelled kinds, whose edges are
/// containment information rather than a ladder to coarsen along.
pub(super) fn lineage_kind(kind: mosaica_types::layer::HierarchyKind) -> Option<bool> {
    match kind {
        mosaica_types::layer::HierarchyKind::Nested => Some(false),
        // A `dag` layer is `nested` with several parents: the cut reads every depth's count
        // rather than bisecting.
        mosaica_types::layer::HierarchyKind::Dag => Some(true),
        _ => None,
    }
}

/// The response-local depth of every served treed artifact — its `rung`: the longest parent
/// chain over the response's own links.
///
/// `parents_of` holds every served row of the treed layers, keyed by `mosaica_id`, valued with
/// `parent_ids`, which only ever name identifiers in the same response and layer. An empty
/// list, or an identifier `parents_of` does not hold, are both roots — rung 0.
///
/// Iterative depth-first, ancestors memoised. The in-progress mark guards a cycle: a malformed
/// store contributes nothing rather than looping.
pub(crate) fn response_rungs(
    parents_of: &std::collections::HashMap<u64, Vec<u64>>,
) -> std::collections::HashMap<u64, u32> {
    const PENDING: u32 = u32::MAX;
    let mut known: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::with_capacity(parents_of.len());
    let mut stack: Vec<u64> = Vec::new();
    for &id in parents_of.keys() {
        if known.contains_key(&id) {
            continue;
        }
        stack.push(id);
        while let Some(&at) = stack.last() {
            let ups = parents_of.get(&at).map_or(&[][..], Vec::as_slice);
            match known.get(&at).copied() {
                None => {
                    known.insert(at, PENDING);
                    for &up in ups {
                        if !known.contains_key(&up) {
                            stack.push(up);
                        }
                    }
                }
                Some(PENDING) => {
                    let rung = ups
                        .iter()
                        .filter_map(|up| known.get(up).copied())
                        .filter(|&k| k != PENDING)
                        .map(|k| k + 1)
                        .max()
                        .unwrap_or(0);
                    known.insert(at, rung);
                    stack.pop();
                }
                Some(_) => {
                    stack.pop();
                }
            }
        }
    }
    known
}

/// Where one served artifact sits, and what it points at. Both edges are recorded during the
/// walk and resolved after it, since whether either end is in the response is not known until
/// every layer and level has been walked.
struct Placement {
    /// Its own address — `(layer, level, ordinal)`.
    at: (String, u32, u32),
    /// The addresses of its parents, where it names any. Within its own layer by construction.
    parents: Vec<(String, u32, u32)>,
    /// The address of the artifact it depends on, where its layer declares a dependency.
    attached_to: Option<(String, u32, u32)>,
}

/// A dependent whose target this response does not contain, and everything hanging from it. The
/// cut runs after the verdicts, so a treed layer under an `artifact_budget`, with a layer
/// depending on it, could otherwise serve a label for a cluster this response does not hold —
/// one response must never contradict itself.
///
/// A request naming only the dependent layer keeps its rows: what this drops is a response that
/// walked the target's layer and did not serve the target, or a target outside this viewport.
/// Not a disclosure control — everything it removes already passed its own verdict.
///
/// Chains cascade. Returns one flag per placement, positionally, and removes what it drops from
/// `served_at` so a dropped artifact cannot be named as anything's parent.
fn orphaned_dependents(
    placed: &[Placement],
    in_request: &std::collections::BTreeSet<String>,
    served_at: &mut std::collections::BTreeMap<(String, u32, u32), MosaicaId>,
    outside: &Outside,
) -> Vec<bool> {
    let mut dependents_of: std::collections::BTreeMap<(&str, u32, u32), Vec<usize>> =
        std::collections::BTreeMap::new();
    let mut queue: Vec<usize> = Vec::new();
    for (i, place) in placed.iter().enumerate() {
        let Some(target) = &place.attached_to else {
            continue;
        };
        dependents_of
            .entry((target.0.as_str(), target.1, target.2))
            .or_default()
            .push(i);
        if in_request.contains(&target.0)
            && !served_at.contains_key(target)
            && !outside.contains_key(target)
        {
            queue.push(i);
        }
    }
    let mut dropped = vec![false; placed.len()];
    while let Some(i) = queue.pop() {
        if dropped[i] {
            continue;
        }
        dropped[i] = true;
        served_at.remove(&placed[i].at);
        // Whatever hung from it goes too — its target's layer is in this request by construction.
        let at = &placed[i].at;
        if let Some(hanging) = dependents_of.get(&(at.0.as_str(), at.1, at.2)) {
            queue.extend(hanging.iter().copied());
        }
    }
    dropped
}

#[cfg(test)]
pub(super) mod fixture {
    //! One shard's rows of a level, and the sets a tile is answered over in each side.

    use std::ops::Range;

    use croaring::Bitmap;

    use super::{spans_rows, ViewportSets};
    use crate::compose::EffectiveMask;
    use crate::tile_index::Viewport;

    /// One shard's rows of a level: each ordinal's members, the rows, what the viewer sees and
    /// the tile's spans there.
    pub(crate) struct Held {
        pub(crate) sets: Vec<Vec<u32>>,
        pub(crate) rows: u32,
        pub(crate) visible: Vec<u32>,
        pub(crate) spans: Vec<Range<u32>>,
    }

    impl Held {
        /// One shard holding `a`'s rows and then `b`'s from `offset`, which is past `a`'s.
        pub(crate) fn joined(a: &Held, b: &Held, offset: u32) -> Held {
            assert!(offset >= a.rows, "the second shard's rows follow the first's");
            fn shifted(rows: &[u32], offset: u32) -> impl Iterator<Item = u32> + '_ {
                rows.iter().map(move |row| row + offset)
            }
            Held {
                sets: a
                    .sets
                    .iter()
                    .zip(&b.sets)
                    .map(|(first, second)| {
                        first.iter().copied().chain(shifted(second, offset)).collect()
                    })
                    .collect(),
                rows: offset + b.rows,
                visible: a.visible.iter().copied().chain(shifted(&b.visible, offset)).collect(),
                spans: a
                    .spans
                    .iter()
                    .cloned()
                    .chain(b.spans.iter().map(|span| span.start + offset..span.end + offset))
                    .collect(),
            }
        }

        pub(crate) fn leaf(&self) -> EffectiveMask {
            EffectiveMask::of_rows(Bitmap::of(&self.visible))
        }

        pub(crate) fn tile_rows(&self) -> Bitmap {
            spans_rows(&self.spans)
        }
    }

    /// The sets of each side at its index in `leaves`, the tile spanning that side's `rows`.
    pub(crate) fn sets_over<'a>(
        leaves: &'a [EffectiveMask],
        rows: &'a [Bitmap],
    ) -> Vec<ViewportSets<'a>> {
        leaves
            .iter()
            .zip(rows)
            .enumerate()
            .map(|(place, (mask, rows))| ViewportSets {
                place,
                mask,
                viewport: Viewport::compose(rows, mask),
                matched_here: None,
                highlighted_here: None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{sets_over, Held};
    use super::*;
    use crate::artifacts::ArtifactRows;
    use crate::layer_read::LevelForms;

    /// The artifacts of a level each of `shards` proposes in its tile rows and holds a visible
    /// member of there, the one at `withheld` withheld by its label.
    fn present_over(shards: &[&Held], withheld: u32) -> Vec<u32> {
        let forms = LevelForms::synthetic(
            shards
                .iter()
                .map(|held| {
                    let sets: Vec<Option<&[u32]>> =
                        held.sets.iter().map(|set| Some(set.as_slice())).collect();
                    ArtifactRows::synthetic(&sets, None)
                })
                .collect(),
        );
        let leaves: Vec<EffectiveMask> = shards.iter().map(|held| held.leaf()).collect();
        let rows: Vec<croaring::Bitmap> = shards.iter().map(|held| held.tile_rows()).collect();
        present_in(&forms, &sets_over(&leaves, &rows), |ordinal| ordinal != withheld)
            .iter()
            .collect()
    }

    #[test]
    fn a_levels_candidates_over_two_shards_are_the_union_of_each_shards() {
        // Ordinal 0 is seen in the tiles only in the first shard, 1 only in the second, 2 in
        // neither, 3 in both, and 4 is withheld by its label.
        let a = Held {
            sets: vec![vec![1, 2], vec![7], vec![9], vec![4, 5], vec![1]],
            rows: 10,
            visible: vec![1, 3, 4, 9],
            spans: vec![Range { start: 0, end: 6 }],
        };
        let b = Held {
            sets: vec![vec![7], vec![2, 3], vec![0], vec![5], vec![2]],
            rows: 8,
            visible: vec![2, 5],
            spans: vec![Range { start: 1, end: 6 }],
        };
        assert_eq!(present_over(&[&a, &b], 4), vec![0, 1, 3]);
        assert_eq!(present_over(&[&a], 4), vec![0, 3]);
        assert_eq!(present_over(&[&b], 4), vec![1, 3]);
        assert_eq!(present_over(&[&Held::joined(&a, &b, 10)], 4), vec![0, 1, 3]);
    }
}
