//! Serving annotation artifacts: the gate, the dependency prerequisite and the response pass.

use super::*;

/// Whether one level of one layer is answered for.
///
/// Split out of the serving loop so the rule is readable on its own and a test can state it
/// directly: the loop's job is to skip, and this is what it skips on.
pub(crate) fn level_is_selected(
    selection: LevelSelection<'_>,
    declared: &[tessera_types::layer::LevelDeclaration],
    level: u32,
    zoom: u8,
) -> bool {
    // **A layer that declares no levels is not selectable, in any of the three forms.** A treed or
    // flat layer sits entirely at level 0 (decision 0082) and a level number names nothing about it,
    // so a request naming levels for the tiered layer beside it must not blank it. Without this the
    // uniform reading — the selection applies to every layer named — makes `levels: [1]` alongside
    // `layers: "all"` serve nothing at all from every clustering in the deployment.
    if declared.is_empty() {
        return true;
    }
    match selection {
        LevelSelection::All => true,
        LevelSelection::Named(levels) => levels.contains(&level),
        LevelSelection::Declared => {
            // Nothing declared a scale, so there is no map to follow and every level answers. This
            // is the treed and flat case, and also the levelled layer whose author declared titles
            // and no ranges.
            if !declared.iter().any(|d| d.zoom.is_some()) {
                return true;
            }
            match declared.iter().find(|d| d.level == level) {
                // Declared, so the range decides.
                Some(d) => match d.zoom {
                    Some((lo, hi)) => (lo..=hi).contains(&u32::from(zoom)),
                    // A level with no range of its own in a layer that has them: no scale to be
                    // outside of.
                    None => true,
                },
                // A run with no declaration behind it — level 0 of a treed layer reached through a
                // layer that also declares levels cannot happen, but a run beyond the declared
                // list would otherwise vanish silently.
                None => true,
            }
        }
    }
}

/// How long a chain of dependencies one request will follow.
///
/// **A backstop, not a limit anyone should reach.** A dependency graph is acyclic by construction —
/// a layer is registered only after every layer it names in `depends_on` — so a real chain is
/// bounded by the number of declared layers and is one or two links deep in practice. This bounds
/// the recursion anyway, because the alternative to a bound on a request path is a stack that a
/// disagreeing store could run off; refusing a chain longer than this withholds artifacts, which is
/// the direction a backstop must fail in.
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
    /// The level's masked counts where it has them, carried so the drill-down's derived geometry
    /// reads the accumulation this gate already built rather than walking the mask again
    /// (`crate::histogram::MaskedGeometry`).
    pub(super) counts: Option<Arc<crate::histogram::MaskedCounts>>,
}

/// One request's state, as the dependency prerequisite needs it.
///
/// Gathered once per response rather than per artifact: every field is a property of the request —
/// the viewer, the generation, the view and the composed mask — and none of them is a property of
/// the artifact being tested.
///
/// **The view is [`ServedView`] whole**, because a dependency's verdict is the *same* verdict and
/// so must be reached with the same inputs: the same generation, the same segments, the same deny
/// mask and the same masked-count key. The two fields beside it are the ones the prerequisite has
/// of its own — the mask it counts against, and the reachability resolved once for this response.
struct DependencyContext<'a> {
    served: &'a ServedView<'a>,
    mask: &'a crate::compose::EffectiveMask,
    reachable: &'a tessera_lifecycle::ResolvedLayers,
}

/// What [`Engine::warm_artifact_projections`] did, for the open's own log line — the shape
/// `crate::shapes::Warmed` already has, and read the same way: a count of structures and a
/// duration, naming no artifact and no principal.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WarmedProjections {
    /// How many `(view, layer, level)` forms were asked for. Every one is either built here or
    /// already held, which at open can only be the second when two views share nothing — so this
    /// is the number of builds unless a level was skipped.
    pub(crate) levels: u64,
    /// What the whole pass took. **The figure that moved off the request path**, and the one an
    /// operator compares against the start they used to get.
    pub(crate) elapsed_ms: u64,
}

impl Engine {
    /// Build every live level's row-space projection **now**, so no request ever pays for one.
    ///
    /// # Why this is at open rather than on the first request that wants it
    ///
    /// `ArtifactProjections::get_or_build` is a cache, and its build is
    /// `RowSpace::project_base` over a level's whole membership — "seconds, not milliseconds" at
    /// corpus scale, and **measured at 23.3 s** for rung 3's `mesh/descriptors`, whose membership
    /// is 1.66×10⁹ rows (`probes/2026-09-02-cold-start/`). Left lazy, that lands on whichever
    /// request of a fresh process happens to be first — a viewport naming the layer, a browse of
    /// it, or a `member_of` highlight over it — and the viewer sees a blank map for half a minute.
    /// It is per **process**, so every restart re-arms it and a demo restarts often.
    ///
    /// Paid here, it is paid before the listeners are bound: `tessera_server::prepare` opens the
    /// engine and `run` binds afterwards, so nothing can reach `/readyz` — let alone a request —
    /// until this returns. **The cost does not disappear; it moves off the request and onto the
    /// start**, which is the trade a restart-often deployment wants and the one an operator can
    /// see, because it is reported below.
    ///
    /// # What it does not do
    ///
    /// **Nothing is materialised per token over the artifact population** (decision 0093). Every
    /// structure built here is per `(view, layer, level)` and shared by every principal: the row
    /// form, its tile index or its label column, and the containment partition. The per-principal
    /// half — the masked counts, the gate, the verdicts — is not touched, and cannot be: there is
    /// no session at open.
    ///
    /// **It holds no more than serving would.** These are exactly the entries the cache would
    /// hold after one request of each shape, under the same replace-on-mismatch rule; what changes
    /// is when they arrive, not how many there are. A deployment whose clients only ever ask for
    /// one of many views does now hold the others' forms — and pays for them at start — which is
    /// the honest cost of the trade.
    ///
    /// **Failure is an absence, not a refusal.** A view no partition carries, a view two carry
    /// (which is a request error in its own right), a layer suppressed at open: each is skipped
    /// and the level is built on first use, which is what every request did before this existed.
    /// Refusing to open over a derived structure that has a correct fallback would be a refusal
    /// outside the disclosure surface.
    ///
    /// ⊘ A fold's prefix rotation does not re-warm: a rotated prefix invalidates every key, and
    /// the level is rebuilt by the first request after it, exactly as before. The engine adopts no
    /// derived structures at a rotation either, so this would be the only half of that pair.
    pub(crate) fn warm_artifact_projections(&self) -> WarmedProjections {
        let started = std::time::Instant::now();
        let mut warmed = WarmedProjections::default();
        let generation = self.generation.load();
        let layers = self.write.live().registered_layers();
        if layers.is_empty() {
            return warmed;
        }
        let source = generation.partition_source();
        // The views this bundle carries, each with the partition that carries it. A view two
        // partitions carry is `EngineError::MultiPartitionView` on the request path; here it is
        // simply not warmed, so the request that hits the error is not preceded by a build for a
        // row space no request will use.
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
                // The same two live tests `serve_artifacts` takes, in the same order: a suppressed
                // or deleted layer is served to nobody, so building its form would be work for a
                // set no response can carry.
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
                    // **The engine's own pool**, for `Engine::masked_counts`' reason: the
                    // projection's decode fans out, and a build outside `install` would take
                    // rayon's global pool rather than the one the deployment sized.
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
                    // **The other two per-generation structures a first request would build**,
                    // and they are here for the row form's reason rather than for their size: the
                    // lineage is ~0.5 s at rung 3's 30,217-node DAG and the level's contents ~0.5 s
                    // beside it, both derived from the level's records alone. Neither depends on a
                    // mask, a viewport or a principal, so neither is work a request should be
                    // doing — and leaving them lazy would leave *some* per-process build on the
                    // first request after the expensive one had been moved.
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
                    // Skipped where the layer declares no supplied content, exactly as the two
                    // serving paths skip it: there is nothing to read.
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

    /// This level's masked counts, where the level is served row-major and so has no other route to
    /// them.
    ///
    /// **`None` on an artifact-major level, and that is not a fallback**: such a level counts one
    /// artifact at a time against the composed mask, which a request's budget bounds.
    ///
    /// **Built lazily, on the first request that needs it** — a whole walk of the mask, which is the
    /// 0.85–1.7 s at 10⁷ artifacts decision 0093 prices. A cold drill-down on a row-major level
    /// therefore pays the level's whole histogram to answer about one artifact, which is stated here
    /// rather than discovered: the column has no per-artifact route to a masked count, so the choice
    /// is between this and re-scanning the mask for every drill-down.
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
        // **The accumulated geometry rides the same walk, on a level that has no other route to
        // it**: a column-only form holds no per-artifact membership, so a centroid or a box taken
        // one artifact at a time costs the visible rows inside that artifact's extent — the whole
        // visible set for a scattered artifact, per served artifact. Where the form holds its
        // bitmaps the per-artifact route is one intersection and this is not built.
        let accumulate = segments.filter(|_| !rows.membership().rows_held());
        Some(self.masked_counts.get_or_build(
            identity.key(view, layer, level, level_version, accumulate.is_some()),
            || {
                // On the engine's own pool, because the walk inside is split across it
                // (`RowColumn::histogram_over`) and a request must not spill onto rayon's
                // global pool, which nothing here sizes.
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

    /// Which layers this principal may know exist — one set probe for a gate-failed name and a
    /// never-registered one alike (`LayerRegistry::resolve_for`). One resolution per response, and
    /// one statement of it: a route that resolved reachability differently from another would be
    /// two answers to one question.
    fn reachable_layers(&self, served: &ServedView<'_>) -> tessera_lifecycle::ResolvedLayers {
        self.write.live().resolve_layers(
            |term| served.session.satisfied().contains(&term),
            |label| served.generation.dict.lookup(label.as_bytes()),
        )
    }

    /// **One artifact, located and gated for one principal** — the predicate
    /// [`Engine::artifact`] answers by, shared with the region leaf by artifact
    /// (`polygon-membership.md` §8) so that a shape a viewer may filter through is exactly a shape
    /// they would be served, by the same call.
    ///
    /// **`None` is the only failure shape.** An identifier naming nothing, one naming a point, one
    /// whose layer this principal does not reach or which is suppressed, one on another view, and
    /// one below its layer's existence criterion are one answer — C17's posture, and what keeps
    /// the leaf by artifact from being an oracle over shapes a viewer was not served.
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

        // Addressing, before authorisation and cheaply: which artifact, if any, this entity is.
        let Some((name, level, ordinal)) = self.write.live().locate_artifact(entity) else {
            return Ok(None);
        };
        let Some(layer) = self.write.live().registered_layer(&name) else {
            return Ok(None);
        };
        if !layer.declaration.views.iter().any(|s| s == view) {
            return Ok(None);
        }
        // **Which view the artifact is of, decided here and not by what a form happens to hold.**
        // On a group-scoped layer an artifact belongs to one view of the group (`views.md` §3.5),
        // so an identifier naming another view's artifact is refused at the point, and at the
        // cost, of one naming nothing — the two 404s are the same answer. Deciding it beside
        // `locate_artifact` rather than at the verdict closes this route whatever state a held row
        // form is in, and builds no projection and no histogram to do it.
        if !self.write.live().with_artifacts(|store| {
            store.drawn_in_view(&name, level, ordinal, crate::artifacts::view_key(view))
        }) {
            return Ok(None);
        }
        // Reachability, then the live suppression of the layer itself — the same two steps in the
        // same order `Engine::visible_layers` and `serve_artifacts` take.
        let reachable = self.reachable_layers(served);
        if !reachable.contains(&name)
            || generation.overlay.is_deleted(layer.entity)
            || generation.overlay.is_suppressed(layer.entity)
        {
            return Ok(None);
        }

        let source = generation.partition_source();
        let recorded = layer.layout_of(level);
        // The predicate's own inputs, resolved once for this identifier — the same rule the
        // viewport resolves per layer, from the same generation, so an artifact reached by
        // identifier and one reached by viewport cannot be evaluated against different memberships.
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
            // **The version the form is of, not the store's.** Between an accepted write and
            // the tick that publishes its delta the two differ, and anything keyed on the store's
            // would name a derivation of a form that has not taken the write
            // (`ArtifactProjections::get_or_build`).
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
        // ⊘ **A cold drill-down on a row-major level pays the level's whole histogram**, because
        // the column has no per-artifact route to a masked count — see `Engine::masked_counts`.
        let counts = self.masked_counts(
            &mask_identity,
            view,
            &name,
            level,
            level_version,
            &rows,
            mask,
            // **Only where the layer's derived content is an accumulation** — a level serving a
            // count alone has no use for a position per visible row.
            crate::artifacts::derives_accumulated_geometry(&layer.declaration)
                .then_some(segments),
        );
        let carried_counts = counts.clone();
        // The same containment answers the viewport builds, from the same partition: an identifier
        // route that resolved containment by a different arm would be a second ranking nobody
        // wrote. Lazily, because this route resolves one identifier — see `answer_for_one`.
        let containment = rows
            .partition()
            .map(|p| p.answer_for_one(session.satisfied()));
        let ctx = DependencyContext {
            served,
            mask,
            reachable: &reachable,
        };
        let dependency_served = self.dependency_gate(&ctx);
        let artifact_view = crate::artifacts::ArtifactView {
            declaration: &layer.declaration,
            overlay: &generation.overlay,
            satisfied: session.satisfied(),
            layer_reachable: true,
            rows: &rows,
            mask,
            dependency_served: &dependency_served,
            containment,
            denied,
            counts,
        };
        // ⊘ Per-artifact terms arrive with content (Stage 3); until then a layer whose
        // `artifact_visibility` names a field withholds here as it does on the viewport, which is
        // the same fail-closed answer reached by the same call.
        let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
            artifact_view.verdict(entity, ordinal, None)
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

    /// Drill down on one artifact by the identifier a response handed out.
    ///
    /// **The same predicate the viewport calls, and that is the whole design of this method.** An
    /// artifact reachable by identifier but not by viewport — or the reverse — is two
    /// transcriptions of one rule, which is the failure mode this codebase has written down more
    /// than once. So this resolves the address, resolves the layer, and then calls
    /// [`crate::artifacts::ArtifactView::verdict`], exactly as `serve_artifacts` does. The only
    /// difference is that there is no tile candidacy: the caller named the artifact.
    ///
    /// **`None` is the only failure shape.** An identifier naming nothing, one naming a point
    /// rather than an artifact, one whose layer this principal does not reach, one whose artifact
    /// is suppressed, and one below its layer's existence criterion are one answer. That last route
    /// reads as new and is not — Appendix C's C17 annotation: the criterion tests the **masked**
    /// count, so it can only cross the bar when this principal's own visible membership changes.
    ///
    /// **On the cost channel.** In the steady state every route here is cheap and comparable: the
    /// session's geometry is resolved from the per-session cache a viewport already filled, and the
    /// membership's row form from the per-deployment cache. The one expensive path — building a
    /// projection — is deployment-wide state keyed on what was published, not on who is asking, so
    /// its timing carries nothing about a principal.
    ///
    /// `zoom` is the depth the caller draws at, for the vertex rule a predicate or an authored
    /// shape is served under (`polygon-membership.md` §7.2); `None` serves the whole presimplified
    /// shape under the budget alone. The derived kind — the hull — is unaffected by it.
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
        // The one predicate, shared with the viewport and with the region leaf by artifact.
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
        // Same resolution as the viewport's, by the same call — an identifier route that served a
        // different content would be a second ranking nobody wrote.
        let Some(content) = self.supplied_content(
            &generation,
            &name,
            level,
            ordinal,
            entity,
            layer.declaration.content.supplied.len(),
            rank,
            true,
            // One artifact, so the direct read: the level's table would answer this in O(1) and
            // cost a pass over the level to build, which is the wrong trade for a route that
            // resolves one identifier.
            None,
        ) else {
            return Ok(None);
        };
        // The same computation the viewport does, from the same composed mask — one route's
        // geometry differing from the other's would be two transcriptions of one rule, which is
        // exactly what the shared predicate above exists to prevent.
        let declared_derived: Vec<crate::derived::ComputedProperty> = layer
            .declaration
            .content
            .computed
            .iter()
            .filter_map(|name| crate::derived::ComputedProperty::parse(name))
            .collect();
        //
        // **The same per-principal cache the viewport reads** (`crate::derived_cache`), and this is
        // the route that most needs it: the client asks the viewport for centroids and this for the
        // one shape it draws (`artifact-shapes.md` §9), so a viewer moving the pointer back over a
        // cluster they have already hovered pays nothing.
        let derived = if declared_derived.is_empty() {
            crate::derived::DerivedContent::default()
        } else {
            let key = crate::derived_cache::DerivedKey {
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
                properties: crate::derived_cache::properties_bits(&declared_derived),
            };
            match counts.as_ref().and_then(|c| c.geometry()) {
                // The accumulation the level's own counts carry — see
                // `crate::histogram::MaskedGeometry`.
                Some(geometry) => {
                    crate::derived::accumulated(&declared_derived, geometry, ordinal)
                }
                None => {
                    let content = self.derived_geometry.get_or_derive(key, || {
                        let Ok(segments) = segments_with_row_bases(view, view_data) else {
                            // Unreachable in practice — the view resolved above — and an empty
                            // content is the fail-closed reading of a row space that cannot be
                            // assembled.
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
        // The one drawn geometry of the other two kinds (`polygon-membership.md` §7.1): this
        // route is asked for the one shape a client draws, so it always answers.
        let mut content = content;
        let mut derived = derived;
        let shape_guard_fired = self.drawn_shape(
            &layer.declaration,
            view,
            &name,
            level,
            ordinal,
            &mut content,
            &mut derived,
            zoom,
        );
        Ok(Some(ArtifactOut {
            content,
            layer: name.clone(),
            tessera_id: id,
            key: self.write.live().with_artifacts(|store| {
                store.get(&name, level, ordinal).and_then(|r| r.key.clone())
            }),
            masked_count,
            derived,
            // The declared level — which is the rung on every layer kind *for this route*: a
            // treed layer's stored level is 0, and its response-local chain depth is also 0 here,
            // this response being one artifact with no parent links to be deep in.
            rung: level,
            // **Always empty on this route, and not by omission.** A parent is named only where
            // it is also in the response, and this response is one artifact — so there is nothing
            // for it to name. Resolving the parents here anyway would hand a caller who holds one
            // identifier the existence of a coarser artifact they were never served.
            parent_ids: Vec::new(),
            // **Always absent on this route, and for `parent_ids`' reason.** A target is named
            // only where it is also in the response, and this response is one artifact — so
            // there is nothing for it to name. Resolving the attachment here anyway would hand a
            // caller who holds one identifier the existence of an artifact they were never
            // served.
            target: None,
            // The identifier route carries no filter to answer about (decision 0104), and there is
            // no viewport for the answer to be scoped to either.
            matched: None,
            // Nor a highlight, for the same two reasons.
            highlighted: None,
            shape_guard_fired,
        }))
    }

    /// **The predicate and the authored kind of an artifact's one drawn geometry**
    /// (`polygon-membership.md` §7.1), filled into `derived.shape` beside the count — the derived
    /// kind, the hull, is already there from [`crate::derived::compute`]. Returns whether the
    /// vertex budget fired.
    ///
    /// A **predicate** shape is the level's held canonical shape at this ordinal
    /// (`crate::shapes`), served at the request's depth (`crate::shapes::served_rings`) — the
    /// same bytes for every principal, which is what `/v1/meta`'s kind tells a client. It is
    /// served under the artifact's own verdict and nothing else: this is reached only for an
    /// artifact that verdict admitted.
    ///
    /// An **authored** shape is the supplied content at the layer's shape slot, which
    /// `supplied_content` already gated by that content's own `require_member_visibility`: the
    /// canonical per-view bytes are read back out of the slot, the request's view's shape is
    /// served at the same rule, and **the slot is blanked** — the wire's `content` carries the
    /// layer's texts, and the geometry travels as rings in `shape_x`/`shape_y`. A slot that does
    /// not read as a shape draws nothing rather than a guess.
    ///
    /// **Never on a request that did not ask**: the caller passes `derived` only where the
    /// request's `computed` selected the shape, and passes the content list only where it was
    /// materialised.
    #[allow(clippy::too_many_arguments)]
    fn drawn_shape(
        &self,
        declaration: &tessera_types::layer::LayerDeclaration,
        view: &str,
        layer: &str,
        level: u32,
        ordinal: u32,
        content: &mut [String],
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
                let Some((slot, _)) = declaration.authored_shape() else {
                    return false;
                };
                let Some(text) = content.get_mut(slot) else {
                    return false;
                };
                let shapes = tessera_lifecycle::membership::ArtifactShapes::from_content_text(text);
                text.clear();
                let Some(shape) = shapes
                    .as_ref()
                    .and_then(|s| s.for_view(view))
                    .and_then(|bytes| tessera_spatial::shape::Shape::decode(bytes).ok())
                else {
                    return false;
                };
                let (parts, guarded) = crate::shapes::served_rings(&shape, zoom);
                derived.shape = Some(parts);
                guarded
            }
        }
    }

    /// The values of the content the predicate chose, or `None` where it chose one whose content
    /// cannot be read back.
    ///
    /// **`rank` is the index into the artifact's ranked `contents`** — not a Morton rank and not a
    /// rank within a bitmap, both of which this module uses the word for elsewhere.
    ///
    /// `Some(vec![])` and `None` are different answers and the difference is the whole point:
    /// the first is *this layer declares no supplied content*, which is most layers; the second is
    /// *this artifact should carry content and it is not here*, which withholds the artifact.
    ///
    /// **`materialise = false` runs the same servability test and copies nothing** — the identity
    /// projection's setting (`artifact-fetch-protocol.md` §5.2). `Some`/`None` is decided by
    /// identical checks on either setting, because that answer withholds the artifact and a
    /// projection must not move the row set; all `false` skips is the string copies, and its
    /// `Some` always carries the empty vector. One function with a flag rather than a probing
    /// sibling, so the two readings of "servable" cannot drift apart.
    ///
    /// **`table` is the level's contents, read once for the level** — the viewport pass supplies
    /// it, and it is what keeps a response of thousands of artifacts off a zstd block read per
    /// artifact (`crate::artifact_content`, which carries the measurement). `None` reads the one
    /// entity's row directly: the drill-down route asks about one artifact, and building a whole
    /// level's table to answer that would trade a block read for a pass over the level.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn supplied_content(
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
        // The publication's own copy, while it is still in memory — the log is the only home the
        // content has between the publish and the manifest that carries it.
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

        // Otherwise the record blob, at this artifact's own entity. Tags are `rank × kinds + kind`
        // against the layer's declaration — see `ArtifactStore::unpublished_content`.
        if kinds == 0 {
            return Some(Vec::new());
        }
        let base = (rank as usize).checked_mul(kinds)?;
        let entity = u32::try_from(entity.raw()).ok()?;
        /// **Every declared kind or none.** A row missing one is content that did not survive its
        /// write, and serving the rest would hand a client an artifact short of what its layer
        /// says it carries — which is indistinguishable, from the client's side, from content
        /// withheld. Stated once, for both routes below.
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
        // **The two routes decide identically**, which is the whole reason the tag walk above is
        // one loop over a lookup rather than two loops: the table holds the row's utf8 fields, and
        // a tag it does not hold is a tag the row did not carry *or* one whose value was not text
        // — both of which withhold on the direct route too.
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

    /// The dependency prerequisite, shared by both serving routes: **is the artifact this one
    /// attaches to served to this viewer?**
    ///
    /// One function rather than two call sites doing the same steps, on the argument the shared
    /// predicate itself rests on: a route that gated dependencies differently from the other would
    /// be two transcriptions of one rule, and the one that drifted would be serving labels for
    /// clusters their viewer cannot see.
    ///
    /// **The target's own `verdict`, not a cheaper summary of it**
    /// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
    /// rule 2). Its layer's gate, its live suppression, its existence, its own terms, its existence
    /// criterion against *this viewer's* masked count, and its containment all decide here, because
    /// "visible" means the same thing for a dependency as it does for anything else. The
    /// conjunction can only narrow, so the term introduces no disclosure of its own.
    ///
    /// **The order matters and is the order the served layer's own path takes**: reachability
    /// first, then the layer's live disposition, then the level and the slot, then the predicate.
    /// A reachability resolved once per session may be cached; a disposition may not, and asking
    /// them in this order is what keeps a layer suppression from being outlived by a session.
    ///
    /// **Recursion, bounded by the declaration graph.** A dependency may itself be a dependent — a
    /// label on a label — and the chain terminates because a layer is registered only after every
    /// layer it names in `depends_on`, which makes the graph acyclic by construction. `depth` is a
    /// backstop for a store that somehow disagrees with that, and it fails closed rather than
    /// deep: a chain longer than any real declaration is refused, not followed.
    fn dependency_served(
        &self,
        ctx: &DependencyContext<'_>,
        attachment: &tessera_lifecycle::membership::Attachment,
        depth: u32,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        // A name this principal does not reach, and a layer dropped since the resolution, are one
        // answer here for the reason they are one answer everywhere: which of them applies is
        // exactly the fact being withheld.
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
        // A layer that does not live in this view has no membership in this row space, so there is
        // nothing here that could be served.
        if !layer.declaration.views.iter().any(|s| s == ctx.served.name) {
            return false;
        }
        let record = self.write.live().with_artifacts(|store| {
            store
                .get(&attachment.layer, attachment.level, attachment.ordinal)
                .map(|record| record.entity)
        });
        // **The slot answers, and it must answer with the entity the edge names.** A hole is what
        // the fold leaves where it executed a deletion — in the same publication that retired the
        // overlay entry saying so — and an ordinal holding a *different* entity is an edge into an
        // artifact that is gone and has been republished over. Both are absent.
        let Some(entity) = record.filter(|entity| *entity == attachment.entity) else {
            return false;
        };
        let recorded = layer.layout_of(attachment.level);
        // The target's own membership, evaluated the same way its own serving route would — a
        // dependency answered from a different rule would be a second membership nobody wrote.
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
            // The version the form is of — `Engine::gated_artifact`'s note, and the same reason.
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
        // The target's own count, from whichever structure its layout puts it in — the same
        // histogram the viewport would read, under the same key, so a dependency answered here and
        // the target answered directly cannot disagree.
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
        // **Lazily, and this one is load-bearing rather than tidy.** The prerequisite runs once per
        // attached candidate, so settling a level's whole expression table here would turn a
        // per-artifact question into whole-population work per artifact.
        let containment = rows
            .partition()
            .map(|p| p.answer_for_one(ctx.served.session.satisfied()));
        crate::artifacts::ArtifactView {
            declaration: &layer.declaration,
            overlay: &ctx.served.generation.overlay,
            satisfied: ctx.served.session.satisfied(),
            layer_reachable: true,
            rows: &rows,
            mask: ctx.mask,
            dependency_served: &nested,
            containment,
            denied: ctx.served.denied,
            counts,
        }
        // ⊘ Per-artifact terms arrive with content, so the target's own label is `None` here
        // exactly as it is on the two serving routes — the same fail-closed answer reached by the
        // same call.
        .verdict(entity, attachment.ordinal, None)
        .is_served()
    }

    /// The prerequisite as the predicate takes it: a closure over one request's state.
    fn dependency_gate<'a>(
        &'a self,
        ctx: &'a DependencyContext<'a>,
    ) -> impl Fn(&tessera_lifecycle::membership::Attachment) -> bool + 'a {
        move |attachment| self.dependency_served(ctx, attachment, DEPENDENCY_CHAIN_MAX)
    }

    /// The artifacts of this viewport: every one the request asked for, that this principal
    /// reaches, that has a visible member inside the requested tiles, and that passes the one
    /// predicate.
    ///
    /// **Four narrowings, in that order, and the order is the disclosure control.** Reachability
    /// first, because it costs one set probe and a name the principal cannot reach must not have
    /// its membership touched at all. Candidacy second, because it is the cheap masked question and
    /// it keeps the count off every artifact outside the viewport. The predicate last, because it
    /// is the one that decides, and it is [`crate::artifacts::ArtifactView::verdict`] — the same
    /// function drill-down and every later route calls.
    ///
    /// **The count is over the whole membership, not over the tiles.** A viewer is told how many of
    /// a cluster's documents they can see, which does not change as they pan; a per-viewport count
    /// would move with the box and let a viewer difference two boxes for the members in between.
    /// Candidacy is the only per-tile question here.
    ///
    /// **The view arrives whole and is not resolved a second time.** The deny mask, the segments
    /// and the masked-count key this pass needs are the ones the point path composed its mask
    /// from, so an artifact and a point of one response cannot be answered over different row
    /// spaces.
    pub(super) fn serve_artifacts(
        &self,
        served: &ServedView<'_>,
        mask: &crate::compose::EffectiveMask,
        tiling: &Tiling,
        req: &ViewportRequest<'_>,
    ) -> Result<(Vec<ArtifactOut>, Vec<ServedLayer>)> {
        // Which layers this principal may know exist — one set probe for a gate-failed name and a
        // never-registered one alike (`LayerRegistry::resolve_for`).
        let reachable = self.reachable_layers(served);
        // **Intersected with the request, never unioned.** A name the principal does not reach is
        // absent whether or not they asked for it, so asking is not a way to learn what exists.
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
        // Built once for the whole response, and from the *same* resolution the names above came
        // from: a label's target may live in any layer its own declares in `depends_on`, reachable
        // or not, and asking a second resolution would be a second answer to one question.
        let ctx = DependencyContext {
            served,
            mask,
            reachable: &reachable,
        };
        let dependency_served = self.dependency_gate(&ctx);

        // The rows this request's tiles span, which every set below is taken over.
        let Some(tile_rows) = tile_rows(served, &tiling.ranges) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let sets = viewport_sets(&tile_rows, mask);

        // **The layers this response walks**, which is what makes the dependent drop below
        // decidable. A target missing from a response that never looked at its layer was not
        // removed from anything — see [`orphaned_dependents`].
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
        // Built once per request rather than per layer: it is the same view's segment list for
        // every artifact in the response, and a layer declaring no derived content never asks it
        // anything.
        let locator = crate::derived::RowLocator::new(served.segments.clone());
        // Built once for the whole response: the postings and the manifest's plugin are the
        // generation's, not the layer's, and the gate they carry is one decision per request.
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
                // **Skipped before the projection is built, not after it is served.** A level the
                // request did not ask for costs nothing at all here: no `get_or_build`, no
                // candidate walk, no masked probe and no derived geometry over its members. That is
                // the whole point of the field — a whole-layer response over a five-level
                // administrative hierarchy pays a pass over every member at every level, and the
                // levels a client was never going to draw dominate it.
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
            // Dropped between the resolution and here. Absent is the right answer and the same
            // one a gate failure gives.
            return None;
        };
        // A layer declares which views it lives in; one it did not declare has no membership
        // in this row space to project.
        if !registered
            .declaration
            .views
            .iter()
            .any(|s| s == pass.served.name)
        {
            return None;
        }
        // **The live half, asked per request.** A layer's own entity carries its suppression,
        // and a resolution may cache reachability but never the verdict — see
        // `Engine::visible_layers`, which takes the same two steps in the same order.
        if generation.overlay.is_deleted(registered.entity)
            || generation.overlay.is_suppressed(registered.entity)
        {
            return None;
        }
        if lineage_kind(registered.declaration.hierarchy.kind).is_some() {
            walked.treed.insert(name.clone());
        }

        // Parsed once per layer. A name outside the vocabulary cannot reach here — the
        // declaration was refused at registration — so an unparseable one is dropped rather
        // than erroring the whole response.
        //
        // **The request narrows it, and only ever narrows it.** The intersection is taken here
        // so that a property the request did not ask for is never computed at all — the point
        // of the field is the work it does not do, and filtering the *result* would keep the
        // hull's cost while dropping its bytes.
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
                // **The form and the version it is of, from the one call.** A form still
                // waiting for a tick's delta stands at the earlier level version, and the
                // masked-count histogram below decides candidacy on a row-major level —
                // so a histogram of this form filed under the store's later version would
                // be read, after the tick, as though it had counted the grown column
                // (`ArtifactProjections::get_or_build`).
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
        // **The count's route, decided by the level's layout and by nothing about the
        // request.** An artifact-major level counts per served artifact; a row-major one has
        // no per-artifact membership to intersect and reads the histogram, which is built
        // once per session per generation and cached under a key that moves with every
        // accepted deny (`crate::histogram`).
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
        // **Built after the candidacy route and never as part of it**: the filter decides
        // nothing about which artifacts are served (decision 0104), so this is computed
        // beside the verdict rather than inside it, and is skipped whole on an unfiltered
        // request.
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
            satisfied: served.session.satisfied(),
            layer_reachable: true,
            rows,
            mask: pass.sets.mask,
            dependency_served: pass.dependency_served,
            containment,
            denied: served.denied,
            // Kept on the level beside this, which takes the `Arc` — the derived geometry reads
            // the accumulation this same entry carries.
            counts: level.counts.clone(),
        };
        // **Every candidate is tested before any is cut**, and the two passes are separate
        // for a reason that is not performance: the verdict is a per-artifact question
        // with no lineage input (decision 0080), and a loop that decided *and* pruned in
        // one step would have the shape that lets a node's neighbours reach its verdict.
        let mut passing = Vec::new();
        // **The walk replaces the sweep over every ordinal.** Cost is the viewport's
        // perimeter in the hierarchy rather than the level's population: an artifact in no
        // node the viewport touches has no member there, so it cannot have a *visible* one
        // and skipping it withholds nothing (`crate::tile_index`, and §4.1 on why this is a
        // candidate generator and never an answer). Holes and artifacts whose membership
        // projects to nothing are in no node either, so neither reaches the predicate here
        // — and both remain live on the identifier route, which walks no index.
        //
        // **Or the scan, where the level is served row-major**: one pass over
        // `viewport ∩ M_auth` marking labels, which answers the same question at a cost in
        // *points* rather than in artifacts (`ArtifactRows::candidacy`). Which route is
        // taken is a property of the level and never of the request.
        let candidates = rows.candidacy(&pass.sets.viewport, level.counts.as_deref());
        for ordinal in candidates.iter() {
            // **Every candidate pays a masked probe**, on whichever of the three routes the
            // classification makes cheapest — see `ArtifactRows::candidate_in`, which is
            // the one place the choice is made and the one the differential drives.
            if !rows.candidate_in(ordinal, &candidates, &pass.sets.viewport, pass.sets.mask) {
                continue;
            }
            let Some(entity) = level.runs.entity_of(ordinal as u64).map(EntityId::new) else {
                continue;
            };
            // ⊘ **No artifact carries its own terms yet**, so a layer whose
            // `artifact_visibility` names a field serves nothing here — fail-closed, and
            // visibly so. The per-artifact label arrives with content (Stage 3); until then
            // the named field has nothing to satisfy, and admitting the artifact instead
            // would make a missing declaration a grant to everyone.
            let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
                view.verdict(entity, ordinal, None)
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
        // The level's lineage, read from the parent lists of **every** artifact and not
        // only the passing ones, because the edges are a property of the level and one
        // lineage serves every viewer. **What the cut is taken over is the passing nodes
        // alone** (decision 0117 E): the plan counts depth in them and climbs through the
        // rest, so a withheld ancestor is not in this viewer's tree.
        //
        // **Within-level edges only, and that is the whole of the tiered shape's
        // treatment here** (owner ruling, 2026-08-18). A tiered layer's edges run
        // between levels and are *information* — what contains what, so a client can nest
        // what it draws or filter to one subtree — rather than a ladder to coarsen along.
        // Climbing them would substitute a state for its counties and draw one large
        // polygon across a region whose neighbours are still counties. So the cut does not
        // see them, such a layer's lineage is empty here, and its budget is inert exactly
        // as a flat layer's is.
        //
        // **Held per generation, not derived per request.** The pointers depend on neither
        // the mask nor the viewport, so a request that rebuilds them is doing generation
        // work: ~96 ms at a level of ten million, against the ~3 ms the cut over them now
        // costs.
        //
        // **Read from the row form, whose records and versions were taken inside one hold
        // of the artifacts lock** (stage one), which is what makes the cached lineage the
        // lineage *of* the version it is filed under: read separately, a write landing
        // between the two would file the new level's edges under the old level's version,
        // and the next request would serve a cut through a tree that has moved.
        //
        // **Filed under the level's lineage version, not its record version**
        // (`ingest.md` §1.5, §4.1): a page of members joining moves the records and not
        // the edges, so it leaves this lineage held; a publication, a parent fill and a
        // retirement move both.
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
        // Ascending and deduplicated, which the cut guarantees — so the membership test in
        // the assembly below is a binary search rather than a scan of the served set once
        // per candidate.
        //
        // **`prune_children` is the layer's, and it is a rendering choice rather than a
        // disclosure one.** Pruned, a passing parent is dropped where a passing child sits
        // beneath it; unpruned, both are served and the client receives the whole visible
        // tree — which is what lets it nest what it draws, or filter to one subtree while
        // still drawing the rest. Every artifact in either set cleared its own criterion,
        // so neither is the safer answer.
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
        // **The level's supplied content, read once for the level rather than once per
        // served artifact** (`crate::artifact_content`, which carries the measurement that
        // put it here: 408 ms of a response whose points half is 1.3 ms, all of it one
        // zstd block decompressed per artifact served — 6.7 ms once the level's contents
        // are read together). Built after the cut, so a level whose artifacts all failed
        // their verdict reads nothing at all, and skipped whole where the layer declares no
        // supplied content — which is most layers.
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

        for (ordinal, entity, masked_count, rank) in passing {
            if cut.binary_search(&ordinal).is_err() {
                continue;
            }
            // D-C: checked once per artifact served. The derived sweep is the response's
            // dominant CPU and it runs between two flushes, so without a checkpoint here a
            // client that has gone — or a stream the server has shed — is discovered only
            // when the whole frame is ready to send: three abandoned GeoNames requests each
            // held a worker for minutes (2026-08-28), deriving geometry nobody would read.
            check_cancelled(&req.cancel)?;
            // The one content this viewer contains, entire. ⊘ A content restored from a
            // packed extent carries no values yet (its content belongs in the record blob,
            // decision 0077, and that write is unbuilt), and is **withheld** rather than
            // served with its description missing.
            //
            // **Asked under the identity projection too, and deliberately** — with
            // `materialise = false`, so the values are not copied but the *servability*
            // test is identical. Content-cannot-be-served withholds the artifact, so
            // skipping the probe here would let an identity response carry a row the full
            // response withholds, breaking §5.2's row-set contract sentence.
            let Some(content) = self.supplied_content(
                generation,
                name,
                number,
                ordinal,
                entity,
                declaration.content.supplied.len(),
                rank,
                req.artifact_rows == ArtifactRows::Full,
                contents.as_deref(),
            ) else {
                continue;
            };
            // The blinding is total over the space the allocator issues, so this cannot
            // fail for an entity that came out of the runs above; a failure would mean the
            // manifest and the allocator disagree, and dropping the artifact is the
            // fail-closed reading of that.
            let Ok(tessera_id) = self.identity_key.forward(pass.shard, entity) else {
                continue;
            };
            // **From the composed mask, and only from it.** The visible rows are the
            // artifact's membership intersected with what this principal may see, so every
            // property below is a function of `membership ∩ M_auth` and nothing else
            // (`annotations.md` §4.2). Skipped entirely where the layer declares nothing,
            // which is what keeps a count-only layer at count-only cost — and skipped
            // whole under the identity projection, which is that projection's point: the
            // derived sweep is the response's dominant CPU and decides nothing about
            // which rows are served (`artifact-fetch-protocol.md` §5.2).
            //
            // **Held per principal between requests** (`crate::derived_cache`): a pan
            // re-serves mostly the same artifacts to the same viewer, and a shape is the
            // most expensive thing this loop does. The key names the principal, so a hit
            // answers the request that would have derived the same value.
            let derived = if req.artifact_rows == ArtifactRows::Identity
                || layer.declared_derived.is_empty()
            {
                crate::derived::DerivedContent::default()
            } else {
                let key = crate::derived_cache::DerivedKey {
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
                    properties: crate::derived_cache::properties_bits(&layer.declared_derived),
                };
                // **The accumulation where the level has one** — see
                // `crate::histogram::MaskedGeometry`. It was built with this request's
                // counts, under the same key, so nothing here walks the mask again.
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
            // The predicate or the authored shape, **only where the request asked for the
            // shape** (`polygon-membership.md` §7.1) and the row is materialised — the
            // identity projection carries no geometry and no content at all.
            let mut content = content;
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
                    &mut content,
                    &mut derived,
                    Some(req.zoom),
                )
            } else {
                false
            };
            // **The parents come from the level's own records and the key from the
            // store.** Both are per-ordinal facts of one generation, but only one of them
            // is held in the row form: a key is a caller's string, one per artifact, and
            // copying ten million of them into a cached structure buys nothing the
            // store's own lookup does not already answer. The key is payload, so the
            // identity projection skips the lookup.
            let parents: Vec<(String, u32, u32)> = rows
                .parents(ordinal)
                .iter()
                .map(|p| (name.clone(), p.level, p.ordinal))
                .collect();
            let key = match req.artifact_rows {
                ArtifactRows::Identity => None,
                ArtifactRows::Full => self
                    .write
                    .live()
                    .with_artifacts(|store| store.get(name, number, ordinal)?.key.clone()),
            };
            // Recorded, not resolved: which artifacts this response holds is not known
            // until every layer and level has been walked, and a parent — or the artifact
            // a dependent hangs from — may sit in a level this loop has not reached.
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
                // The declared level. On a treed layer — where every artifact sits at
                // level 0 and the rung is the response-local chain depth — this is
                // recomputed below, once the response's row set is final.
                rung: number,
                // Both filled in below, once the response's own membership is settled.
                parent_ids: Vec::new(),
                target: None,
                // Asked only of the artifacts that survived the cut: the bit describes what
                // is served, and an artifact the response drops has no row to carry one.
                matched: level.matched.as_ref().map(|m| rows.matches(m, ordinal)),
                // 0104's probe with the highlight's crossed set in place of the filter's
                // — the same early-exiting intersection, over `all_of[filters, highlight]`
                // (`highlight-and-hierarchy.md` §2).
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
    /// The dependency prerequisite, closed over this request's state — one closure for the
    /// response, called per attached candidate.
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
    /// **The predicate's inputs, resolved per level.** An attribute layer's every level
    /// reads the same column; a spatial layer's held structures are per level, because
    /// each level holds its own shapes. A layer with a stored membership resolves nothing.
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
    /// The level's masked counts where it has them, and the accumulated geometry with them where
    /// the layer derives one — `None` on an artifact-major level.
    counts: Option<Arc<crate::histogram::MaskedCounts>>,
    /// The filter's and the highlight's answers over this level, each taken from the one set the
    /// request composed and so borrowed from it rather than from the row form.
    matched: Option<crate::artifacts::Matched<'a>>,
    highlighted: Option<crate::artifacts::Matched<'a>>,
}

/// The row-space sets one response is answered over, composed once for every layer in it.
struct ViewportSets<'a> {
    /// The viewer's composed mask, carried beside the sets taken from it so the walk cannot be
    /// handed one without the others.
    mask: &'a crate::compose::EffectiveMask,
    /// The request's tiles as one masked set — the candidate generator every level walks.
    viewport: crate::tile_index::Viewport<'a>,
    matched_here: Option<croaring::Bitmap>,
    highlighted_here: Option<croaring::Bitmap>,
}

/// The viewport as one row-space set, built once for every layer: the merged global spans of
/// every tile this request resolved. `crossing_domain` already merges and globalises them for the
/// filter's crossing, and reusing it is what keeps the two from disagreeing about which rows a
/// request covers.
///
/// `None` where those tiles span no row at all: no layer can have a candidate there, and the
/// response carries no artifacts frame.
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
    // **The one composition, hoisted out of every layer and every artifact**
    // (`design/artifact-serving-at-scale.md` §4 step 2, and `crate::tile_index::Viewport`).
    // Built once per request: it is the same set for every layer in the response, and its cost
    // is the viewport's containers rather than the population's.
    let viewport = crate::tile_index::Viewport::compose(tile_rows, mask);
    // **The filter's half of the same hoisting, and it is composed once for the request too**:
    // `viewport ∩ M_auth ∩ M_sel`, the one set decision 0104's bit is asked against. `None` is
    // an unfiltered request — no question, and no column on the wire to answer it.
    let matched_here = mask.matched_rows(viewport.here());
    // **The highlight's half of the same hoisting**: `viewport ∩ M_auth ∩ M_sel ∩ highlight`,
    // the one set the conjunction's bit is asked against (`highlight-and-hierarchy.md` §2).
    // `None` is a request carrying no highlight — no question, and a null column to say so.
    let highlighted_here = mask.highlighted_rows(viewport.here());
    ViewportSets {
        mask,
        viewport,
        matched_here,
        highlighted_here,
    }
}

/// What one walk of the requested layers produced, before the response's own membership settles
/// which of it is served.
///
/// **Held outside the level it is written from**, which is what lets the assembly write here while
/// the level's row form, counts and contents are still borrowed.
#[derive(Default)]
struct Walked {
    out: Vec<ArtifactOut>,
    /// The layers whose rung is the response-local chain depth rather than the declared level —
    /// the treed (nested) ones, decision 0082's edges-not-levels shape. Collected during the
    /// walk, applied after the response's row set is final.
    treed: std::collections::BTreeSet<String>,
    /// Where each served artifact ended up — collected during the walk and reconciled after it.
    served_at: std::collections::BTreeMap<(String, u32, u32), TesseraId>,
    /// Positionally aligned with `out`: where each artifact sits, and what it points at.
    placed: Vec<Placement>,
    /// Every level this pass walked, with the structures the membership column reads the served
    /// set back through — the same row form and the same lineage the verdicts and the cut used, so
    /// the column cannot describe a level the artifacts frame did not.
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
    // **The cut ran after the verdicts, so a dependent may have passed on a target this
    // response then removed.** Dropping it here — before the parents are resolved, so a
    // dependent that goes takes its own name out of `served_at` with it — is what keeps one
    // response from describing a cluster it does not contain (decision 0089).
    let dropped = orphaned_dependents(&placed, in_request, &mut served_at);

    // **A dependent's masked count is its own** (owner ruling, 2026-09-18): the count of the
    // membership it is served over, which by
    // [decision 0145](../../../docs/decisions/0145-an-attached-artifact-with-no-members-of-its-own-is-served-over-its-targets-membership.md)
    // is its target's membership where it declares none of its own and its own generating set
    // otherwise. The copy this pass used to make — a label's row carrying its cluster's
    // number — is gone with the client's join by count: a label names its target by
    // identifier now, and a count beside a label is not a feature.
    //
    // **The two filter bits are still copied** (decision 0104;
    // `highlight-and-hierarchy.md` §2 for the second). A label describes its cluster, so *does
    // anything here match* is a question about the cluster; the label's own membership is often
    // empty, and a bit over it would read `false` for every label under every filter — the same
    // defect the count rule exists to prevent, in the fields beside it. Derivable from the
    // target's own row in this response, which the drop above guarantees is present, so it
    // discloses nothing new (decision 0023).
    //
    // **The two travel as one pair, deliberately.** `highlighted` is `matched` under a second
    // expression and not a second kind of answer, so a shape that let one inherit and the
    // other keep the label's own would serve two answers to one question about one cluster —
    // which is what happened when this carried `matched` alone.
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

    // **A parent is named only where it is also in this response**, which is the whole of the
    // disclosure rule for this field, and it applies per entry (C29). An artifact whose parent
    // exists but was withheld — below its own criterion for this viewer, suppressed, or dropped
    // by the frontier — carries no entry for it, indistinguishable from a root having none.
    // Naming it would tell the viewer that a coarser grouping exists which they are not
    // cleared to see, which is a disclosure the rest of this pass takes care to avoid making.
    // Ascending by identifier, so a client that wants one parent takes the first and gets the
    // same one every time.
    //
    // **A target is named on exactly the same terms** (owner ruling, 2026-09-18): the
    // identifier this response served the target under, and nothing where the response holds
    // no row for it. `served_at` is the resolved set the drop above already pruned, so a
    // dependent that survived finds its target there and one whose target's layer was never
    // in the request finds nothing — the second being the *give me just the labels* request,
    // which is answered as it always was.
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
    // **A treed layer's rung is the response-local depth** — the longest parent chain to each
    // row in the forest this response's own `parent_ids` links form
    // (`artifact-fetch-protocol.md` §5.3, `dag-hierarchies.md` §5). Computed here, after the
    // cut, the content withholds and the dependent drop, because those are what make the
    // forest response-local: a row whose ancestors were pruned, withheld or cut away is a root
    // of its subtree and reads 0, whatever its depth in the stored tree.
    //
    // **Settled before the membership column's served set is handed over**, because the
    // column ranks by this rung and never by the stored depth: the stored depth counts
    // withheld nodes, so two served artifacts holding one point would be ordered by an
    // artifact the viewer cannot see (`dag-hierarchies.md` §6, decision 0117 E).
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
    // **The membership column's served set is `served_at` after the drop** — exactly the
    // artifacts in `served`, and the only identifiers the column can name — each with its
    // response-local rung (0 on a layer whose rung is its declared level, which the level
    // index already ranks).
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
/// `Some(dag)` for the kinds whose edges are roll-up, `None` for the flat and levelled kinds,
/// whose edges are information rather than a ladder to coarsen along
/// ([decision 0087](../../../docs/decisions/0087-cross-level-edges-are-information-not-rollup.md)).
fn lineage_kind(kind: tessera_types::layer::HierarchyKind) -> Option<bool> {
    match kind {
        tessera_types::layer::HierarchyKind::Nested => Some(false),
        // A `dag` layer is `nested` with several parents (decision 0117): a lineage, and one the
        // cut reads every depth's count over rather than bisecting (`dag-hierarchies.md` §6).
        tessera_types::layer::HierarchyKind::Dag => Some(true),
        _ => None,
    }
}

/// The response-local depth of every served treed artifact — its `rung`
/// (`artifact-fetch-protocol.md` §5.3): **the longest parent chain** to it over the response's
/// own links (`dag-hierarchies.md` §5), the same definition the stored lineage's depth has.
///
/// `parents_of` holds every served row of the treed layers, keyed by `tessera_id`, valued with the
/// response's own `parent_ids` — which, by that field's contract, only ever name identifiers in
/// the same response, and within the artifact's own layer. An empty list, and an identifier
/// `parents_of` does not hold, are both roots: *no parent in this response* is rung 0, whatever
/// the stored tree says.
///
/// One depth-first pass with the ancestors memoised, iterative for the same reason
/// [`crate::cut::Lineage`]'s is; the cycle guard is the in-progress mark — the publish and the
/// mint refuse a cycle, so a parent still in progress when its child is resolved is a malformed
/// store, and it contributes nothing rather than looping.
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

/// Where one served artifact sits, and what it points at.
///
/// Both edges are recorded during the walk and resolved after it, because whether either end is in
/// the response is not known until every layer and level has been walked.
struct Placement {
    /// Its own address — `(layer, level, ordinal)`, the triple an [`Attachment`] carries.
    at: (String, u32, u32),
    /// The addresses of its parents, where it names any. Within its own layer by construction.
    parents: Vec<(String, u32, u32)>,
    /// The address of the artifact it depends on, where its layer declares a dependency.
    attached_to: Option<(String, u32, u32)>,
}

/// **A dependent whose target this response does not contain, and everything hanging from it.**
///
/// [Decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)
/// makes a dependent visible exactly where its target is, and `Engine::dependency_served` enforces
/// that by asking the target's own `verdict`. **The cut runs after the verdicts** — it serves fewer
/// artifacts and never evaluates fewer — so a request carrying an `artifact_budget` over a treed
/// layer, alongside a layer depending on it, would otherwise be answered with labels describing
/// clusters that same response does not hold. One response never contradicts itself.
///
/// **This pass is what makes the wire's `target` total.** The attachment does reach the wire since
/// the owner's ruling of 2026-09-18 — as the `tessera_id` of the target's row in the same response
/// — and the objection the earlier rule answered was never to publishing it but to naming an
/// artifact the response does not contain. Dropping the dependent here removes that case: what is
/// left to name is always a row this response carries, which is the rule `parent_ids` follows.
///
/// **The target's layer must be in this request.** A request naming the dependent layer *alone* —
/// "give me just the labels" — finds no target here, and a naive lookup would drop every label.
/// That is a legitimate call and refusing it is outside the disclosure surface; such a request
/// behaves exactly as it did before this pass existed. What the condition catches is a response
/// that walked the target's layer and did not serve the target: cut to a budget, pruned in favour
/// of a child, or withheld at the emit step. A target outside this viewport falls under the same
/// rule and is dropped with them — its label is describing something this response does not draw,
/// and separating the two cases would mean carrying a reason per absent candidate through a pass
/// that deliberately collapses reasons.
///
/// **This does not make the budget a disclosure control**
/// ([decision 0083](../../../docs/decisions/0083-the-frontier-is-a-request-time-budget.md) stands).
/// The pass can only remove, and everything it removes already passed its own test. It decides what
/// is *drawn*, and a label describing something not drawn is not drawn either.
///
/// Chains cascade: a label on a label goes when the label it hangs from goes. The worklist walks
/// the edges the response holds rather than rescanning it per drop, and terminates because each
/// index is dropped at most once — a chain deeper than `DEPENDENCY_CHAIN_MAX` was already refused
/// by the prerequisite that admitted these artifacts in the first place.
///
/// Returns one flag per placement, positionally, and removes what it drops from `served_at` so a
/// dropped artifact cannot be named as anything's parent.
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
        // Whatever hung from it goes too, and its target's layer is in this request by
        // construction — this response walked the layer, which is how the artifact reached `out`.
        let at = &placed[i].at;
        if let Some(hanging) = dependents_of.get(&(at.0.as_str(), at.1, at.2)) {
            queue.extend(hanging.iter().copied());
        }
    }
    dropped
}
