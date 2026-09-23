//! Serving annotation artifacts: the gate, the dependency prerequisite and the response pass.

use super::*;

/// Whether one level of one layer is answered for.
pub(crate) fn level_is_selected(
    selection: LevelSelection<'_>,
    declared: &[tessera_types::layer::LevelDeclaration],
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
    declaration: &tessera_types::layer::LayerDeclaration,
    content: &mut [String],
) -> Option<tessera_lifecycle::membership::ArtifactShapes> {
    let (slot, _) = declaration.authored_shape()?;
    let text = content.get_mut(slot)?;
    let shapes = tessera_lifecycle::membership::ArtifactShapes::from_content_text(text);
    text.clear();
    shapes
}

/// An artifact's supplied content as it may leave the engine: the values, with an authored
/// shape's slot blank, and the shapes taken from that slot.
pub(crate) struct Supplied {
    pub(crate) values: Vec<String>,
    pub(crate) authored: Option<tessera_lifecycle::membership::ArtifactShapes>,
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
}

/// A drawn geometry as the wire carries it: parts, then rings, then vertices in grid units.
pub(crate) type Rings = Vec<Vec<Vec<[u32; 2]>>>;

/// One view's authored shape as rings for the wire, and whether the vertex budget fired; `None`
/// where the artifact authored none for `view` or its bytes do not decode.
pub(crate) fn authored_rings(
    shapes: &tessera_lifecycle::membership::ArtifactShapes,
    view: &str,
    zoom: Option<u8>,
) -> Option<(Rings, bool)> {
    let shape = tessera_spatial::shape::Shape::decode(shapes.for_view(view)?).ok()?;
    Some(crate::shapes::served_rings(&shape, zoom))
}

/// How long a chain of dependencies one request will follow.
///
/// The dependency graph is acyclic by construction, so a real chain is one or two links deep.
/// This is a backstop against a disagreeing store: past it, a request refuses the chain rather
/// than risk unbounded recursion.
const DEPENDENCY_CHAIN_MAX: u32 = 16;

/// What [`Engine::gated_artifact`] answers: the artifact, located, with its level's row form and
/// the verdict's two outputs.
pub(super) struct GatedArtifact {
    pub(super) name: String,
    pub(super) level: u32,
    pub(super) ordinal: u32,
    pub(super) entity: tessera_types::EntityId,
    pub(super) layer: tessera_types::layer::RegisteredLayer,
    pub(super) rows: Arc<crate::artifacts::ArtifactRows>,
    pub(super) masked_count: u64,
    pub(super) rank: Option<u32>,
    /// The level's masked counts where it has them, so derived geometry reuses this gate's
    /// walk of the mask instead of repeating it.
    pub(super) counts: Option<Arc<crate::histogram::MaskedCounts>>,
}

/// One request's state, as the dependency prerequisite needs it: gathered once per response,
/// since every field is a property of the request rather than of the artifact under test.
///
/// The view is [`ServedView`] whole because a dependency's verdict must be reached with the same
/// generation, segments and deny mask as the artifact it is attached to — otherwise the two could
/// disagree about what the viewer sees.
pub(crate) struct DependencyContext<'a> {
    served: &'a ServedView<'a>,
    mask: &'a crate::compose::EffectiveMask,
    reachable: &'a tessera_lifecycle::ResolvedLayers,
    /// Each target layer's label test for this viewer, settled once per request: a named default
    /// is put through the plugin once rather than once per candidate.
    labels: std::cell::RefCell<rustc_hash::FxHashMap<String, crate::artifacts::LabelGate<'a>>>,
}

impl<'a> DependencyContext<'a> {
    pub(crate) fn new(
        served: &'a ServedView<'a>,
        mask: &'a crate::compose::EffectiveMask,
        reachable: &'a tessera_lifecycle::ResolvedLayers,
    ) -> Self {
        DependencyContext {
            served,
            mask,
            reachable,
            labels: Default::default(),
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
    /// A view a partition does not carry, a view two partitions carry, or a layer suppressed at
    /// open are each skipped: the level is then built on first use, as before this existed.
    pub(crate) fn warm_artifact_projections(&self) -> WarmedProjections {
        let started = std::time::Instant::now();
        let mut warmed = WarmedProjections::default();
        let generation = self.generation.load();
        let layers = self.write.live().registered_layers();
        if layers.is_empty() {
            return warmed;
        }
        let source = generation.partition_source();
        // Views this bundle carries, each with the partition that carries it. A view two
        // partitions carry errors on the request path, so it is left unwarmed here.
        let mut views: std::collections::BTreeMap<&str, Option<&tessera_store::read::ViewData>> =
            std::collections::BTreeMap::new();
        for partition in generation.bundle.partitions.values() {
            for (name, data) in &partition.views {
                views
                    .entry(name.as_str())
                    .and_modify(|held| *held = None)
                    .or_insert(Some(data));
            }
        }
        for (view, view_data) in views {
            let Some(view_data) = view_data else { continue };
            let Ok(segments) = segments_with_row_bases(view, view_data) else {
                continue;
            };
            for layer in &layers {
                if !layer.declaration.views.iter().any(|s| s == view) {
                    continue;
                }
                // Same order as `serve_artifacts`: a suppressed or deleted layer is served to
                // nobody, so its form is not worth building.
                if generation.overlay.is_deleted(layer.entity)
                    || generation.overlay.is_suppressed(layer.entity)
                {
                    continue;
                }
                let vocabulary = predicate_vocabulary(&generation, &layer.declaration);
                let code_of_key = |key: &str| match vocabulary {
                    Some(vocabulary) => vocabulary.code_of(key),
                    None => key.parse::<u32>().ok(),
                };
                for level in 0..layer.runs.len() as u32 {
                    let recorded = layer.layout_of(level);
                    // The engine's own pool: the decode fans out, and running it outside
                    // `install` would spill onto rayon's global pool instead of the one sized
                    // for this deployment.
                    let ((rows, level_version), lineage_version) = self.pool.install(|| {
                        self.write.live().with_artifacts(|store| {
                            let predicate = predicate_source(
                                &layer.declaration,
                                &generation,
                                view,
                                view_data,
                                &segments,
                                &code_of_key,
                                &self.shapes,
                                store,
                                level,
                            );
                            (
                                self.artifact_projections.get_or_build(
                                    &generation.prefix,
                                    view,
                                    &layer.declaration.name,
                                    level,
                                    store,
                                    &view_data.row_space,
                                    Some(&source),
                                    recorded,
                                    predicate.as_ref(),
                                    generation.segments_version,
                                    crate::artifacts::serves_column_only(&layer.declaration),
                                ),
                                store.lineage_version(&layer.declaration.name, level),
                            )
                        })
                    });
                    warmed.levels += 1;
                    // Warmed for the same reason as the row form: derived from the level's
                    // records alone (~0.5s each at rung 3's largest), not from a mask or principal.
                    self.lineages.get_or_build(
                        &layer.declaration.name,
                        level,
                        lineage_version,
                        || {
                            let records = rows.records();
                            let edges = (0..records.len() as u32).map(|ordinal| {
                                let within = records
                                    .parents(ordinal)
                                    .iter()
                                    .filter(move |parent| parent.level == level)
                                    .map(|parent| parent.ordinal);
                                (ordinal, within)
                            });
                            match lineage_kind(layer.declaration.hierarchy.kind) {
                                Some(true) => crate::cut::Lineage::dag(edges),
                                _ => crate::cut::Lineage::new(edges),
                            }
                        });
                    // Skipped where the layer declares no supplied content: nothing to read.
                    if !layer.declaration.content.supplied.is_empty() {
                        if let Some(runs) = layer.runs.get(level as usize) {
                            self.level_contents.get_or_build(
                                &layer.declaration.name,
                                level,
                                level_version,
                                generation.segments_version,
                                || {
                                    crate::artifact_content::LevelContent::build(
                                        generation.filter_columns.records(),
                                        runs,
                                    )
                                },
                            );
                        }
                    }
                }
            }
        }
        warmed.elapsed_ms = started.elapsed().as_millis() as u64;
        warmed
    }

    /// This level's masked counts, computed from the composed mask — never from the whole
    /// population, which would reveal a count of items this viewer cannot see. `None` on an
    /// artifact-major level, which counts one artifact at a time instead.
    ///
    /// Built lazily: a whole walk of the mask costs 0.85-1.7s at 10^7 artifacts, so a cold
    /// drill-down on a row-major level pays it once to answer about one artifact.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn masked_counts(
        &self,
        identity: &crate::histogram::MaskIdentity,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
        rows: &crate::artifacts::ArtifactRows,
        mask: &crate::compose::EffectiveMask,
        segments: Option<&[(&SegmentData, u32)]>,
    ) -> Option<Arc<crate::histogram::MaskedCounts>> {
        let column = rows.column()?;
        // A column-only form holds no per-artifact membership, so its geometry rides this walk
        // rather than being read per artifact; where the form holds bitmaps this is skipped.
        let accumulate = segments.filter(|_| !rows.membership().rows_held());
        Some(self.masked_counts.get_or_build(
            identity.key(view, layer, level, level_version, accumulate.is_some()),
            || {
                // On the engine's own pool: the walk splits across it and must not spill onto
                // rayon's global pool.
                self.pool.install(|| match accumulate {
                    None => crate::histogram::MaskedCounts::new(column.histogram(mask)),
                    Some(segments) => {
                        use crate::compose::WholeMask;
                        let locator = crate::derived::RowLocator::new(segments.to_vec());
                        let visible = mask.visible_all();
                        let counts = column.histogram_over(visible);
                        let acc = column.accumulate_over(visible, &|row| locator.position(row));
                        crate::histogram::MaskedCounts::with_geometry(
                            counts,
                            crate::histogram::MaskedGeometry::new(acc.counts, acc.sums, acc.boxes),
                        )
                    }
                })
            },
        ))
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
        mask: &EffectiveMask,
        id: TesseraId,
    ) -> Result<Option<GatedArtifact>> {
        let (session, generation) = (served.session, served.generation);
        let (view, view_data) = (served.name, served.data);
        let (segments, denied) = (&served.segments[..], served.denied);
        let mask_identity = served.mask_identity;
        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        let Some((name, level, ordinal)) = self.write.live().locate_artifact(entity) else {
            return Ok(None);
        };
        let Some(layer) = self.write.live().registered_layer(&name) else {
            return Ok(None);
        };
        if !layer.declaration.views.iter().any(|s| s == view) {
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
        if !reachable.contains(&name)
            || generation.overlay.is_deleted(layer.entity)
            || generation.overlay.is_suppressed(layer.entity)
        {
            return Ok(None);
        }

        let source = generation.partition_source();
        let recorded = layer.layout_of(level);
        // Resolved from the same generation the viewport uses: same membership, either route.
        let vocabulary = predicate_vocabulary(generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let (rows, level_version) = self.write.live().with_artifacts(|store| {
            let predicate = predicate_source(
                &layer.declaration,
                generation,
                view,
                view_data,
                segments,
                &code_of_key,
                &self.shapes,
                store,
                level,
            );
            // The version the form is of, not the store's: they differ between an accepted write
            // and the tick that publishes it.
            self.artifact_projections.get_or_build(
                &generation.prefix,
                view,
                &name,
                level,
                store,
                &view_data.row_space,
                Some(&source),
                recorded,
                predicate.as_ref(),
                generation.segments_version,
                crate::artifacts::serves_column_only(&layer.declaration),
            )
        });
        // See `Engine::masked_counts`: a cold row-major drill-down pays the whole histogram.
        let counts = self.masked_counts(
            &mask_identity,
            view,
            &name,
            level,
            level_version,
            &rows,
            mask,
            // Only where the layer derives accumulated geometry — a count-only level has no use
            // for a position per visible row.
            crate::artifacts::derives_accumulated_geometry(&layer.declaration)
                .then_some(segments),
        );
        let carried_counts = counts.clone();
        // The same containment the viewport builds, from the same partition.
        let containment = rows
            .partition()
            .map(|p| p.answer_for_one(session.satisfied()));
        let ctx = DependencyContext::new(served, mask, &reachable);
        let dependency_served = self.dependency_gate(&ctx);
        let artifact_view = crate::artifacts::ArtifactView {
            declaration: &layer.declaration,
            overlay: &generation.overlay,
            labels: self.label_gate(session, &layer.declaration),
            layer_reachable: true,
            rows: &rows,
            mask,
            dependency_served: &dependency_served,
            containment,
            denied,
            counts,
        };
        let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
            artifact_view.verdict(entity, ordinal)
        else {
            return Ok(None);
        };
        Ok(Some(GatedArtifact {
            name,
            level,
            ordinal,
            entity,
            layer,
            rows,
            masked_count,
            rank,
            counts: carried_counts,
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
        id: TesseraId,
        idset: Option<u32>,
        view: &str,
        zoom: Option<u8>,
    ) -> Result<Option<ArtifactOut>> {
        let generation = self.generation.load_full();
        if let Some(e) = idset {
            if e != generation.bundle.manifest.identity.idset {
                return Err(EngineError::StaleIdSet);
            }
        }
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

        let mut probe = Probe::new();
        let geometry =
            self.session_geometry(session, &generation, view, view_data, &None, &mut probe)?;
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
        let mask_identity = self.mask_identity(session, &generation, &geometry);
        let served = ServedView {
            session,
            generation: &generation,
            name: view,
            data: view_data,
            segments: segments_with_row_bases(view, view_data)?,
            denied,
            mask_identity,
        };
        let Some(gated) = self.gated_artifact(&served, &mask, id)?
        else {
            return Ok(None);
        };
        let GatedArtifact {
            name,
            level,
            ordinal,
            entity,
            layer,
            rows,
            masked_count,
            rank,
            counts,
        } = gated;
        // Same call as the viewport's, so the two routes cannot serve different content.
        let Some(supplied) = self.supplied_content(
            &generation,
            &layer.declaration,
            level,
            ordinal,
            entity,
            rank,
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
                level_version: self
                    .write
                    .live()
                    .with_artifacts(|store| store.level_version(&name, level)),
                segments_version: mask_identity.segments_version,
                overlay_version: mask_identity.overlay_version,
                fragment_identity: mask_identity.fragment_identity,
                fragment_watermark: mask_identity.fragment_watermark,
                properties: crate::derived::cache::properties_bits(&declared_derived),
            };
            match counts.as_ref().and_then(|c| c.geometry()) {
                // The accumulation the level's own counts carry.
                Some(geometry) => {
                    crate::derived::accumulated(&declared_derived, geometry, ordinal)
                }
                None => {
                    let content = self.derived_geometry.get_or_derive(key, || {
                        let Ok(segments) = segments_with_row_bases(view, view_data) else {
                            // Unreachable in practice; fail closed with empty content otherwise.
                            return crate::derived::DerivedContent::default();
                        };
                        let locator = crate::derived::RowLocator::new(segments);
                        let visible = rows.visible_rows(ordinal, &mask);
                        crate::derived::compute(&declared_derived, &visible, &locator)
                    });
                    (*content).clone()
                }
            }
        };
        // The one shape a client draws; this route always answers for it.
        let mut derived = derived;
        let shape_guard_fired = self.drawn_shape(
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
            tessera_id: id,
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
            shape_guard_fired,
        }))
    }

    /// The predicate or authored kind of an artifact's one drawn geometry, filled into
    /// `derived.shape` beside the count. Returns whether the vertex budget fired.
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
        declaration: &tessera_types::layer::LayerDeclaration,
        view: &str,
        layer: &str,
        level: u32,
        ordinal: u32,
        authored: Option<&tessera_lifecycle::membership::ArtifactShapes>,
        derived: &mut crate::derived::DerivedContent,
        zoom: Option<u8>,
    ) -> bool {
        match declaration.drawn_shape() {
            None | Some(crate::shapes::DrawnShape::Derived) => false,
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
                let Some(shape) = held.shapes.get(ordinal as usize).and_then(|s| s.as_ref()) else {
                    return false;
                };
                let (parts, guarded) = crate::shapes::served_rings(&shape.shape, zoom);
                derived.shape = Some(parts);
                guarded
            }
            Some(crate::shapes::DrawnShape::Authored) => {
                let Some((parts, guarded)) =
                    authored.and_then(|shapes| authored_rings(shapes, view, zoom))
                else {
                    return false;
                };
                derived.shape = Some(parts);
                guarded
            }
        }
    }

    /// The values of the content the predicate chose, or `None` where it cannot be read back.
    /// `rank` indexes the artifact's ranked `contents`, not a Morton or bitmap rank.
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
        generation: &crate::Generation,
        declaration: &tessera_types::layer::LayerDeclaration,
        level: u32,
        ordinal: u32,
        entity: EntityId,
        rank: Option<u32>,
        materialise: bool,
        table: Option<&crate::artifact_content::LevelContent>,
    ) -> Option<Supplied> {
        let mut values = self.supplied_values(
            generation,
            &declaration.name,
            level,
            ordinal,
            entity,
            declaration.content.supplied.len(),
            rank,
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
        generation: &crate::Generation,
        layer: &str,
        level: u32,
        ordinal: u32,
        entity: EntityId,
        kinds: usize,
        rank: Option<u32>,
        materialise: bool,
        table: Option<&crate::artifact_content::LevelContent>,
    ) -> Option<Vec<String>> {
        let Some(rank) = rank else {
            return Some(Vec::new());
        };
        // The publication's own copy, the only home the content has before the manifest carries it.
        let held = self.write.live().with_artifacts(|store| {
            store
                .get(layer, level, ordinal)
                .and_then(|record| record.contents.get(rank as usize))
                .and_then(|set| match (&set.values, materialise) {
                    (Some(values), true) => Some(values.clone()),
                    (Some(_), false) => Some(Vec::new()),
                    (None, _) => None,
                })
        });
        if let Some(values) = held {
            return Some(values);
        }

        // Otherwise the record blob. Tags are `rank × kinds + kind` against the declaration.
        if kinds == 0 {
            return Some(Vec::new());
        }
        let base = (rank as usize).checked_mul(kinds)?;
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
                let fields = generation
                    .filter_columns
                    .records()
                    .fields_of(entity)
                    .ok()??;
                values_for(base, kinds, materialise, |tag| {
                    fields
                        .iter()
                        .find(|f| f.tag == tag)
                        .and_then(|f| match &f.value {
                            tessera_filter::RecordValue::Utf8(text) => Some(text.as_str()),
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
        attachment: &tessera_lifecycle::membership::Attachment,
        depth: u32,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        // Unreachable and dropped-since-resolution are one answer: which applies is the fact withheld.
        if !ctx.reachable.contains(&attachment.layer) {
            return false;
        }
        let Some(layer) = self.write.live().registered_layer(&attachment.layer) else {
            return false;
        };
        if ctx.served.generation.overlay.is_deleted(layer.entity)
            || ctx.served.generation.overlay.is_suppressed(layer.entity)
        {
            return false;
        }
        // A layer not in this view has no membership here to serve.
        if !layer.declaration.views.iter().any(|s| s == ctx.served.name) {
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
        let recorded = layer.layout_of(attachment.level);
        // The target's own membership, evaluated the same way its serving route would.
        let vocabulary = predicate_vocabulary(ctx.served.generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let (rows, level_version) = self.write.live().with_artifacts(|store| {
            let predicate = predicate_source(
                &layer.declaration,
                ctx.served.generation,
                ctx.served.name,
                ctx.served.data,
                &ctx.served.segments,
                &code_of_key,
                &self.shapes,
                store,
                attachment.level,
            );
            self.artifact_projections.get_or_build(
                &ctx.served.generation.prefix,
                ctx.served.name,
                &attachment.layer,
                attachment.level,
                store,
                &ctx.served.data.row_space,
                Some(&ctx.served.generation.partition_source()),
                recorded,
                predicate.as_ref(),
                ctx.served.generation.segments_version,
                crate::artifacts::serves_column_only(&layer.declaration),
            )
        });
        // The target's own count, under the same key the viewport would read.
        let counts = self.masked_counts(
            &ctx.served.mask_identity,
            ctx.served.name,
            &attachment.layer,
            attachment.level,
            level_version,
            &rows,
            ctx.mask,
            // A prerequisite asks whether the target is *served*, never for its geometry.
            None,
        );
        let nested = |a: &tessera_lifecycle::membership::Attachment| {
            self.dependency_served(ctx, a, depth - 1)
        };
        // Lazily and load-bearing: runs once per candidate, so settling a whole expression table
        // here would turn a per-artifact question into whole-population work.
        let containment = rows
            .partition()
            .map(|p| p.answer_for_one(ctx.served.session.satisfied()));
        // A statement of its own, so the borrow ends before the verdict below follows a chain
        // back into this function.
        let labels = *ctx
            .labels
            .borrow_mut()
            .entry(attachment.layer.clone())
            .or_insert_with(|| self.label_gate(ctx.served.session, &layer.declaration));
        crate::artifacts::ArtifactView {
            declaration: &layer.declaration,
            overlay: &ctx.served.generation.overlay,
            labels,
            layer_reachable: true,
            rows: &rows,
            mask: ctx.mask,
            dependency_served: &nested,
            containment,
            denied: ctx.served.denied,
            counts,
        }
        .verdict(entity, attachment.ordinal)
        .is_served()
    }

    /// The prerequisite as the predicate takes it: a closure over one request's state.
    pub(crate) fn dependency_gate<'a>(
        &'a self,
        ctx: &'a DependencyContext<'a>,
    ) -> impl Fn(&tessera_lifecycle::membership::Attachment) -> bool + 'a {
        move |attachment| self.dependency_served(ctx, attachment, DEPENDENCY_CHAIN_MAX)
    }

    /// The artifacts of this viewport: every one requested, that this principal reaches, that
    /// has a visible member inside the requested tiles, and that passes the predicate.
    ///
    /// Four narrowings in this order, and the order is the disclosure control: reachability
    /// first, so a name the principal cannot reach never has its membership touched; candidacy
    /// second, keeping the count off artifacts outside the viewport; the predicate last.
    ///
    /// The count is over the whole membership, not the tiles: a per-viewport count would move as
    /// the viewer pans and let them difference two boxes for the members in between.
    ///
    /// The view arrives whole and is not resolved a second time, so an artifact and a point of
    /// one response are answered over the same row space.
    pub(super) fn serve_artifacts(
        &self,
        served: &ServedView<'_>,
        mask: &crate::compose::EffectiveMask,
        tiling: &Tiling,
        req: &ViewportRequest<'_>,
    ) -> Result<(Vec<ArtifactOut>, Vec<ServedLayer>)> {
        let reachable = self.reachable_layers(served.session);
        // Intersected with the request, never unioned: asking for a name is not a way to learn it.
        let names: Vec<String> = match req.layers {
            LayerSelection::Named(list) => list
                .iter()
                .filter(|name| reachable.contains(name))
                .map(|name| name.to_string())
                .collect(),
            LayerSelection::All => reachable.names().map(str::to_string).collect(),
        };
        if names.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        // Built from the same resolution: a label's target may live in any layer its own
        // `depends_on` names, reachable or not.
        let ctx = DependencyContext::new(served, mask, &reachable);
        let dependency_served = self.dependency_gate(&ctx);

        // The rows this request's tiles span, which every set below is taken over.
        let Some(tile_rows) = tile_rows(served, &tiling.ranges) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let sets = viewport_sets(&tile_rows, mask);

        // The layers this response walks — see [`orphaned_dependents`].
        let in_request: std::collections::BTreeSet<String> = names.iter().cloned().collect();

        let walked = self.walk_layers(served, req, names, &sets, &dependency_served)?;
        Ok(settle_response(walked, &in_request))
    }

    /// One pass over the requested layers and the levels this request selects of each: the gate,
    /// the masked count, the cut, and the row every surviving artifact is served as — with where
    /// it sits and what it points at recorded for the reconciliation after the walk.
    fn walk_layers(
        &self,
        served: &ServedView<'_>,
        req: &ViewportRequest<'_>,
        names: Vec<String>,
        sets: &ViewportSets<'_>,
        dependency_served: &dyn Fn(&tessera_lifecycle::membership::Attachment) -> bool,
    ) -> Result<Walked> {
        // Built once per request: the same segment list for every artifact in the response.
        let locator = crate::derived::RowLocator::new(served.segments.clone());
        // The postings and plugin are the generation's, not the layer's.
        let source = served.generation.partition_source();
        let pass = ArtifactPass {
            served,
            req,
            sets,
            dependency_served,
            locator: &locator,
            source: &source,
            shard: served.generation.bundle.manifest.identity.shard_id,
        };

        let mut walked = Walked::default();
        for name in names {
            let Some(layer) = self.layer_pass(&pass, name, &mut walked) else {
                continue;
            };
            let mut served_levels: Vec<ServedLevel> = Vec::new();
            for (number, runs) in layer.registered.runs.iter().enumerate() {
                let number = number as u32;
                // Skipped before the projection is built: costs nothing here, which matters for
                // a whole-layer response over a multi-level hierarchy.
                if !level_is_selected(
                    req.levels,
                    &layer.registered.declaration.levels,
                    number,
                    req.zoom,
                ) {
                    continue;
                }
                let level = self.level_pass(&layer, number, runs);
                let passing = self.gate_candidates(&level);
                let (lineage, cut) = self.cut_level(&level, &passing);
                served_levels.push(ServedLevel {
                    level: number,
                    rows: Arc::clone(&level.rows),
                    lineage: Arc::clone(&lineage),
                    // Filled once the response's membership is settled, below.
                    served: std::collections::HashMap::new(),
                });
                self.assemble_level(&level, passing, &cut, &mut walked)?;
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
    fn layer_pass<'a>(
        &self,
        pass: &'a ArtifactPass<'a>,
        name: String,
        walked: &mut Walked,
    ) -> Option<LayerPass<'a>> {
        let generation = pass.served.generation;
        let Some(registered) = self.write.live().registered_layer(&name) else {
            // Dropped between the resolution and here — the same answer as a gate failure.
            return None;
        };
        // A layer not declaring this view has no membership here to project.
        if !registered
            .declaration
            .views
            .iter()
            .any(|s| s == pass.served.name)
        {
            return None;
        }
        // Asked per request: reachability may be cached but a suppression verdict may not.
        if generation.overlay.is_deleted(registered.entity)
            || generation.overlay.is_suppressed(registered.entity)
        {
            return None;
        }
        if lineage_kind(registered.declaration.hierarchy.kind).is_some() {
            walked.treed.insert(name.clone());
        }

        // Parsed once per layer, an unparseable name dropped rather than erroring the response,
        // and narrowed by the request so a property not asked for is never computed.
        let declared_derived: Vec<crate::derived::ComputedProperty> = registered
            .declaration
            .content
            .computed
            .iter()
            .filter_map(|name| crate::derived::ComputedProperty::parse(name))
            .filter(|property| pass.req.computed.selects(*property))
            .collect();

        let vocabulary = predicate_vocabulary(generation, &registered.declaration);
        Some(LayerPass {
            pass,
            name,
            registered,
            declared_derived,
            vocabulary,
        })
    }

    /// **Stage one: this level, resolved for this request** — the row form and the version it is
    /// of, the level's masked counts, and the two filter sets. Everything the gate, the cut and the
    /// assembly read of the level is settled here and read from there.
    fn level_pass<'a>(
        &self,
        layer: &'a LayerPass<'a>,
        level: u32,
        runs: &'a tessera_types::layer::ReservedRuns,
    ) -> LevelPass<'a> {
        let (pass, served) = (layer.pass, layer.pass.served);
        let generation = served.generation;
        let code_of_key = |key: &str| match layer.vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let recorded = layer.registered.layout_of(level);
        let ((rows, level_version), lineage_version) = self.write.live().with_artifacts(|store| {
            let predicate = predicate_source(
                &layer.registered.declaration,
                generation,
                served.name,
                served.data,
                &served.segments,
                &code_of_key,
                &self.shapes,
                store,
                level,
            );
            (
                // The form and the version it is of, from one call: filing its histogram under
                // a later store version would read it as counting a column it has not.
                self.artifact_projections.get_or_build(
                    &generation.prefix,
                    served.name,
                    &layer.name,
                    level,
                    store,
                    &served.data.row_space,
                    Some(pass.source),
                    recorded,
                    predicate.as_ref(),
                    generation.segments_version,
                    crate::artifacts::serves_column_only(&layer.registered.declaration),
                ),
                store.lineage_version(&layer.name, level),
            )
        });
        // Decided by the level's layout, never the request: artifact-major counts per artifact,
        // row-major reads the histogram.
        let counts = self.masked_counts(
            &served.mask_identity,
            served.name,
            &layer.name,
            level,
            level_version,
            &rows,
            pass.sets.mask,
            crate::artifacts::derives_accumulated_geometry(&layer.registered.declaration)
                .then_some(&served.segments[..]),
        );
        // Built after candidacy: the filter decides nothing about which artifacts are served.
        let matched = pass
            .sets
            .matched_here
            .as_ref()
            .map(|here| rows.matched(here));
        let highlighted = pass
            .sets
            .highlighted_here
            .as_ref()
            .map(|here| rows.matched(here));
        LevelPass {
            layer,
            level,
            runs,
            rows,
            level_version,
            lineage_version,
            counts,
            matched,
            highlighted,
        }
    }

    /// **Stage two: the verdict, for every candidate the viewport touches** — the ordinals that
    /// pass, with the two outputs their verdict carried.
    fn gate_candidates(&self, level: &LevelPass<'_>) -> Vec<Passing> {
        let (layer, pass) = (level.layer, level.layer.pass);
        let served = pass.served;
        let rows: &crate::artifacts::ArtifactRows = &level.rows;
        let containment = rows
            .partition()
            .map(|p| p.answers(served.session.satisfied()));
        let view = crate::artifacts::ArtifactView {
            declaration: &layer.registered.declaration,
            overlay: &served.generation.overlay,
            labels: self.label_gate(served.session, &layer.registered.declaration),
            layer_reachable: true,
            rows,
            mask: pass.sets.mask,
            dependency_served: pass.dependency_served,
            containment,
            denied: served.denied,
            // Kept on the level beside this, so derived geometry reads the same accumulation.
            counts: level.counts.clone(),
        };
        // Every candidate is tested before any is cut: a loop that decided and pruned in one
        // step could let a node's neighbours reach its verdict.
        let mut passing = Vec::new();
        // Replaces a sweep over every ordinal: an artifact in no node the viewport touches has
        // no visible member, so skipping it withholds nothing. Holes and artifacts with no
        // projected membership stay live on the identifier route, which walks no index. Or, on
        // a row-major level, a scan over the viewport intersected with the mask.
        let candidates = rows.candidacy(&pass.sets.viewport, level.counts.as_deref());
        for ordinal in candidates.iter() {
            // An artifact its own label withholds has no membership probed.
            if !view.admits_label(ordinal) {
                continue;
            }
            // Every candidate pays a masked probe, on whichever route is cheapest for it.
            if !rows.candidate_in(ordinal, &candidates, &pass.sets.viewport, pass.sets.mask) {
                continue;
            }
            let Some(entity) = level.runs.entity_of(ordinal as u64).map(EntityId::new) else {
                continue;
            };
            let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
                view.verdict(entity, ordinal)
            else {
                continue;
            };
            passing.push((ordinal, entity, masked_count, rank));
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
        let number = level.level;
        // Read from every artifact's parent list, not only the passing ones, since one lineage
        // serves every viewer; the cut itself is taken over the passing nodes alone, so a
        // withheld ancestor never appears in this viewer's tree. Within-level edges only: a
        // tiered layer's cross-level edges are containment information, not a ladder to coarsen
        // along, so such a layer's lineage is empty here. Held per generation (rebuilding per
        // request would cost ~96ms at ten million against ~3ms for the cut).
        let lineage = self
            .lineages
            .get_or_build(&layer.name, number, level.lineage_version, || {
                let records = level.rows.records();
                let edges = (0..records.len() as u32).map(|ordinal| {
                    let within = records
                        .parents(ordinal)
                        .iter()
                        .filter(move |parent| parent.level == number)
                        .map(|parent| parent.ordinal);
                    (ordinal, within)
                });
                match lineage_kind(layer.registered.declaration.hierarchy.kind) {
                    Some(true) => crate::cut::Lineage::dag(edges),
                    _ => crate::cut::Lineage::new(edges),
                }
            });
        let ordinals: Vec<u32> = passing.iter().map(|&(o, ..)| o).collect();
        // Ascending and deduplicated, so the assembly's membership test is a binary search.
        // `prune_children` is a rendering choice, not a disclosure one: every artifact in either
        // set cleared its own criterion.
        let served = crate::cut::cut(
            &lineage,
            &ordinals,
            layer.pass.req.artifact_budget,
            layer.registered.declaration.hierarchy.prune_children,
        );
        (lineage, served)
    }

    /// **Stage four: the row every survivor is served as** — its content, its derived shape, its
    /// two edges recorded for the reconciliation after the walk, and the placement they resolve
    /// against.
    fn assemble_level(
        &self,
        level: &LevelPass<'_>,
        passing: Vec<Passing>,
        cut: &[u32],
        walked: &mut Walked,
    ) -> Result<()> {
        let (layer, pass) = (level.layer, level.layer.pass);
        let (served, req) = (pass.served, pass.req);
        let generation = served.generation;
        let declaration = &layer.registered.declaration;
        let (name, number) = (&layer.name, level.level);
        let rows: &crate::artifacts::ArtifactRows = &level.rows;
        // Read once for the level rather than once per artifact: 408ms of a response whose
        // points half is 1.3ms, decompressing one zstd block per artifact — 6.7ms read together.
        let contents = if declaration.content.supplied.is_empty() || cut.is_empty() {
            None
        } else {
            Some(self.level_contents.get_or_build(
                name,
                number,
                level.level_version,
                generation.segments_version,
                || {
                    crate::artifact_content::LevelContent::build(
                        generation.filter_columns.records(),
                        level.runs,
                    )
                },
            ))
        };

        // A level is served in key order, then its keyless artifacts in ordinal order, so a level
        // a build published and one published at a running service are served alike. The keys
        // are read from the level's slots, looked up once, and copied only where the row carries
        // one.
        let survivors: Vec<Passing> = passing
            .into_iter()
            .filter(|&(ordinal, ..)| cut.binary_search(&ordinal).is_ok())
            .collect();
        let full = req.artifact_rows == ArtifactRows::Full;
        let keyed: Vec<(Option<String>, Passing)> = self.write.live().with_artifacts(|store| {
            let slots = store.slots(name, number);
            let key_of = |ordinal: u32| {
                slots
                    .get(ordinal as usize)
                    .and_then(Option::as_ref)
                    .and_then(|record| record.key.as_deref())
            };
            let mut order: Vec<(Option<&str>, Passing)> =
                survivors.into_iter().map(|at| (key_of(at.0), at)).collect();
            order.sort_unstable_by(|(a, at), (b, bt)| {
                (a.is_none(), a, at.0).cmp(&(b.is_none(), b, bt.0))
            });
            order
                .into_iter()
                .map(|(key, at)| (key.filter(|_| full).map(str::to_string), at))
                .collect()
        });
        for (key, (ordinal, entity, masked_count, rank)) in keyed {
            // Checked once per artifact served: without this a client that has gone is
            // discovered only once the whole frame is ready, after minutes deriving geometry
            // nobody reads.
            check_cancelled(&req.cancel)?;
            // Content restored from a packed extent carries no values yet, and is withheld
            // rather than served with its description missing. Asked under the identity
            // projection too, with `materialise = false`: an identity response must not carry a
            // row the full response would withhold.
            let Some(supplied) = self.supplied_content(
                generation,
                declaration,
                number,
                ordinal,
                entity,
                rank,
                req.artifact_rows == ArtifactRows::Full,
                contents.as_deref(),
            ) else {
                continue;
            };
            // The blinding is total over the allocator's space; a failure means the manifest and
            // allocator disagree, and dropping the artifact is the fail-closed reading of that.
            let Ok(tessera_id) = self.identity_key.forward(pass.shard, entity) else {
                continue;
            };
            // From the composed mask, and only from it: every property below is a function of
            // the artifact's membership intersected with what this principal may see. Skipped
            // under the identity projection, whose point is that the derived sweep decides
            // nothing about which rows are served. Held per principal between requests: the
            // cache key names the principal so a hit never answers for a different one.
            let derived = if req.artifact_rows == ArtifactRows::Identity
                || layer.declared_derived.is_empty()
            {
                crate::derived::DerivedContent::default()
            } else {
                let key = crate::derived::cache::DerivedKey {
                    token_id: served.mask_identity.token_id,
                    view: served.name.to_string(),
                    layer: name.clone(),
                    level: number,
                    ordinal,
                    level_version: level.level_version,
                    segments_version: served.mask_identity.segments_version,
                    overlay_version: served.mask_identity.overlay_version,
                    fragment_identity: served.mask_identity.fragment_identity,
                    fragment_watermark: served.mask_identity.fragment_watermark,
                    properties: crate::derived::cache::properties_bits(&layer.declared_derived),
                };
                // The accumulation where the level has one; nothing here walks the mask again.
                match level.counts.as_ref().and_then(|c| c.geometry()) {
                    Some(geometry) => {
                        crate::derived::accumulated(&layer.declared_derived, geometry, ordinal)
                    }
                    None => (*self.derived_geometry.get_or_derive(key, || {
                        let visible = rows.visible_rows(ordinal, pass.sets.mask);
                        crate::derived::compute(&layer.declared_derived, &visible, pass.locator)
                    }))
                    .clone(),
                }
            };
            // Only where the request asked for it and the row is materialised.
            let content = supplied.values;
            let mut derived = derived;
            let shape_guard_fired = if req.artifact_rows == ArtifactRows::Full
                && req.computed.selects(crate::derived::ComputedProperty::Hull)
            {
                self.drawn_shape(
                    declaration,
                    served.name,
                    name,
                    number,
                    ordinal,
                    supplied.authored.as_ref(),
                    &mut derived,
                    Some(req.zoom),
                )
            } else {
                false
            };
            let parents: Vec<(String, u32, u32)> = rows
                .parents(ordinal)
                .iter()
                .map(|p| (name.clone(), p.level, p.ordinal))
                .collect();
            // Recorded, not resolved: a parent or attachment target may sit in a level this
            // loop has not reached yet.
            walked
                .served_at
                .insert((name.clone(), number, ordinal), tessera_id);
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
                tessera_id,
                key,
                masked_count,
                derived,
                // On a treed layer this is recomputed below as the response-local chain depth.
                rung: number,
                // Both filled in below, once the response's own membership is settled.
                parent_ids: Vec::new(),
                target: None,
                matched: level.matched.as_ref().map(|m| rows.matches(m, ordinal)),
                // The same probe, with the highlight's crossed set in place of the filter's.
                highlighted: level.highlighted.as_ref().map(|m| rows.matches(m, ordinal)),
                shape_guard_fired,
            });
        }
        Ok(())
    }
}

/// One artifact the gate admitted: its ordinal, its entity, the masked count its verdict carried
/// and the rank of the content that verdict chose.
type Passing = (u32, EntityId, u64, Option<u32>);

/// **The artifacts pass of one request**: what every layer and every level of it is answered
/// against, resolved once before the walk.
struct ArtifactPass<'a> {
    served: &'a ServedView<'a>,
    req: &'a ViewportRequest<'a>,
    sets: &'a ViewportSets<'a>,
    /// The dependency prerequisite, closed over this request's state.
    dependency_served: &'a dyn Fn(&tessera_lifecycle::membership::Attachment) -> bool,
    locator: &'a crate::derived::RowLocator<'a>,
    source: &'a crate::containment::PartitionSource<'a>,
    shard: u32,
}

/// One layer of that pass: the registration this walk reads it through, and what is parsed once
/// for it.
struct LayerPass<'a> {
    pass: &'a ArtifactPass<'a>,
    name: String,
    registered: tessera_types::layer::RegisteredLayer,
    /// The derived properties this layer declares **and** this request asked for.
    declared_derived: Vec<crate::derived::ComputedProperty>,
    /// The predicate's inputs, resolved per level: per-level for a spatial layer, nothing for a
    /// stored-membership one.
    vocabulary: Option<&'a tessera_store::vocabulary::VocabularyMinter>,
}

/// One level of one layer, as [`Engine::level_pass`] settles it: the values the gate, the cut and
/// the assembly all read, so that none of them resolves one of its own.
struct LevelPass<'a> {
    layer: &'a LayerPass<'a>,
    level: u32,
    /// The level's reserved entity runs — ordinal to entity.
    runs: &'a tessera_types::layer::ReservedRuns,
    /// This view's row form of the level's membership, and the version it is of.
    rows: Arc<crate::artifacts::ArtifactRows>,
    level_version: u64,
    lineage_version: u64,
    /// The level's masked counts and accumulated geometry where it has them — `None` on an
    /// artifact-major level.
    counts: Option<Arc<crate::histogram::MaskedCounts>>,
    /// The filter's and the highlight's answers over this level, borrowed from the request's
    /// composed sets.
    matched: Option<crate::artifacts::Matched<'a>>,
    highlighted: Option<crate::artifacts::Matched<'a>>,
}

/// The row-space sets one response is answered over, composed once for every layer in it.
struct ViewportSets<'a> {
    /// The viewer's composed mask, carried beside the sets taken from it.
    mask: &'a crate::compose::EffectiveMask,
    /// The request's tiles as one masked set — the candidate generator every level walks.
    viewport: crate::tile_index::Viewport<'a>,
    matched_here: Option<croaring::Bitmap>,
    highlighted_here: Option<croaring::Bitmap>,
}

/// The viewport as one row-space set, built once for every layer: the merged global spans of
/// every tile this request resolved, reusing `crossing_domain` so this and the filter's crossing
/// cannot disagree about which rows a request covers.
///
/// `None` where those tiles span no row: no layer can have a candidate there.
fn tile_rows(
    served: &ServedView<'_>,
    ranges: &[Vec<(usize, Range<u32>)>],
) -> Option<croaring::Bitmap> {
    let row_bases: Vec<u32> = served.segments.iter().map(|&(_, base)| base).collect();
    let mut tile_rows = croaring::Bitmap::new();
    for span in crossing_domain(ranges, &row_bases) {
        tile_rows.add_range(span);
    }
    (!tile_rows.is_empty()).then_some(tile_rows)
}

/// The three sets every layer of the response is answered against, composed once from the rows its
/// tiles span.
fn viewport_sets<'a>(
    tile_rows: &'a croaring::Bitmap,
    mask: &'a crate::compose::EffectiveMask,
) -> ViewportSets<'a> {
    // Hoisted out of every layer and every artifact, built once per request.
    let viewport = crate::tile_index::Viewport::compose(tile_rows, mask);
    // `None` is an unfiltered request — no column on the wire.
    let matched_here = mask.matched_rows(viewport.here());
    // `None` is a request carrying no highlight.
    let highlighted_here = mask.highlighted_rows(viewport.here());
    ViewportSets {
        mask,
        viewport,
        matched_here,
        highlighted_here,
    }
}

/// What one walk of the requested layers produced, before the response's own membership settles
/// which of it is served. Held outside the level it is written from, so the assembly can write
/// here while the level's row form, counts and contents are still borrowed.
#[derive(Default)]
struct Walked {
    out: Vec<ArtifactOut>,
    /// The treed layers, whose rung is the response-local chain depth, applied after the row
    /// set is final.
    treed: std::collections::BTreeSet<String>,
    served_at: std::collections::BTreeMap<(String, u32, u32), TesseraId>,
    /// Positionally aligned with `out`: where each artifact sits, and what it points at.
    placed: Vec<Placement>,
    served_layers: Vec<ServedLayer>,
}

/// Reconcile the walk against itself: drop the dependents whose target this response does not
/// hold, resolve the two edges that may only name rows the response carries, and rank the treed
/// layers over the forest that is left.
fn settle_response(
    walked: Walked,
    in_request: &std::collections::BTreeSet<String>,
) -> (Vec<ArtifactOut>, Vec<ServedLayer>) {
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
    let dropped = orphaned_dependents(&placed, in_request, &mut served_at);

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
                .and_then(|target| bits_at.get(target).copied())
        })
        .collect();

    // A parent, and a target, are named only where also in this response — the whole disclosure
    // rule for both fields. One withheld — below its own criterion, suppressed, or dropped by
    // the budget — carries no entry, indistinguishable from a root or an unattached row; naming
    // it would tell the viewer a coarser artifact exists that they are not cleared to see.
    let mut served = Vec::with_capacity(out.len());
    for (((mut artifact, place), dropped), target_bit) in out
        .into_iter()
        .zip(&placed)
        .zip(dropped)
        .zip(target_bits)
    {
        if dropped {
            continue;
        }
        artifact.target = place
            .attached_to
            .as_ref()
            .and_then(|target| served_at.get(target))
            .copied();
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
                a.tessera_id.raw(),
                a.parent_ids.iter().map(|p| p.raw()).collect(),
            )
        })
        .collect();
    let rungs = response_rungs(&parents_of);
    for artifact in served.iter_mut().filter(|a| treed.contains(&a.layer)) {
        artifact.rung = rungs.get(&artifact.tessera_id.raw()).copied().unwrap_or(0);
    }
    // The membership column's served set is `served_at` after the drop: exactly the artifacts
    // in `served`, each with its response-local rung.
    for ((name, level, ordinal), tessera_id) in &served_at {
        if let Some(slot) = served_layers
            .iter_mut()
            .find(|l| &l.name == name)
            .and_then(|l| l.levels.iter_mut().find(|l| l.level == *level))
        {
            let rung = rungs.get(&tessera_id.raw()).copied().unwrap_or(0);
            slot.served.insert(*ordinal, (*tessera_id, rung));
        }
    }
    (served, served_layers)
}

/// Whether a layer's kind holds a lineage the cut climbs and the rung is counted over —
/// `Some(dag)` for roll-up kinds, `None` for flat and levelled kinds, whose edges are
/// containment information rather than a ladder to coarsen along.
fn lineage_kind(kind: tessera_types::layer::HierarchyKind) -> Option<bool> {
    match kind {
        tessera_types::layer::HierarchyKind::Nested => Some(false),
        // A `dag` layer is `nested` with several parents: the cut reads every depth's count
        // rather than bisecting.
        tessera_types::layer::HierarchyKind::Dag => Some(true),
        _ => None,
    }
}

/// The response-local depth of every served treed artifact — its `rung`: the longest parent
/// chain over the response's own links.
///
/// `parents_of` holds every served row of the treed layers, keyed by `tessera_id`, valued with
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
    served_at: &mut std::collections::BTreeMap<(String, u32, u32), TesseraId>,
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
        if in_request.contains(&target.0) && !served_at.contains_key(target) {
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
