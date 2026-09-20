//! The session's geometry for a view: the projection ladder, the composed mask and occupancy.

use super::*;

/// What a request resolves before it counts anything: the view it is served from, this session's
/// geometry over it, the composed mask and the columns the response renders.
pub(super) struct OpenView<'a> {
    pub(super) served: ServedView<'a>,
    /// The composed mask, before any filter narrows it.
    pub(super) mask: EffectiveMask,
    /// Kept beside the mask for the background ladder fill, which takes the whole entry.
    pub(super) geometry: Arc<SessionGeometry>,
    pub(super) coordinates: ViewCoordinates,
    /// The bundle-wide render columns and then this view's scoped ones, in that order.
    pub(super) render_scalars: Vec<DeclaredScalar>,
}

/// θ's inputs for one request: the composed visible cardinality the threshold is anchored on, and
/// the threshold itself. `v_total` outlives the threshold because the filter's route rule reads
/// it too.
pub(super) struct Theta {
    pub(super) v_total: u64,
    pub(super) threshold: Threshold,
}

impl Engine {
    /// Resolve the view this request is served from, and everything fixed by that resolution.
    pub(super) fn open_view<'a>(
        &self,
        session: &'a Session,
        generation: &'a Generation,
        view: &'a str,
        cancel: &Option<CancelToken>,
        probe: &mut Probe,
    ) -> Result<OpenView<'a>> {
        // Refused rather than resolved to one partition: theta's anchor and every rank must be
        // session-global. Unreachable while the build emits one partition; kept so a bundle
        // spanning more is not served half-masked with no error.
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

        // Every segment of the view, each with where its rows begin in the view's row space.
        let segments = segments_with_row_bases(view, view_data)?;
        probe.lap(|t| &mut t.view_lookup_ns);

        // Every flush rotates this cache key for every live session; rebuilding it here would
        // cost on the order of a second, far above a request's budget. A background refresh
        // builds it instead; `session_geometry`'s three-rung ladder finds what that produced.
        //
        // Resolved once, before the parallel tile sweep begins, and only borrowed after by every
        // `tile_sweep` call. Nothing reachable from a rayon worker may touch this cache: a worker
        // blocking on a build only the calling thread can drive would deadlock the pool.
        let geometry = self.session_geometry(session, generation, view, view_data, cancel, probe)?;
        let coordinates = self.view_coordinates(generation, &geometry, view);
        // Names the fragment this request composes against, which under stale-serve is the
        // entry's and not the newest one.
        let mask_identity = self.mask_identity(session, generation, &geometry);
        let base = Arc::clone(&geometry.projection);
        probe.lap(|t| &mut t.row_projection_ns);

        // Render columns only: the full declared-scalar list includes filter-only and
        // blob-resident columns absent from `columns.arrow`, and serving it would misalign a
        // render column's values under the wrong name, since the head's names are zipped
        // positionally with the gathered buffers.
        let mut render_scalars: Vec<_> = generation
            .bundle
            .manifest
            .render_scalars()
            .cloned()
            .collect();
        // Then this view's scoped render columns, and only this view's: a group-scoped `render`
        // attribute occupies a slot in the row tail of every view of its group and no other.
        render_scalars.extend(scoped_render_scalars(
            &generation.bundle.manifest,
            view,
            session.visible_views(),
        ));

        // Checkpoint before compose, placed after the row-projection build so a cancellation
        // observed here never interrupts it — only the work this request would go on to do.
        check_cancelled(cancel)?;

        // Fail-closed on a missing entry: every view the bundle carries has one, empty when
        // nothing is denied, so an absent key means the mask and the bundle disagree about what
        // this generation holds. Treating that as "nothing is denied" would serve suppressed and
        // deleted rows with no error.
        let denied = generation
            .denied()
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;

        // Everything this request's view fixes, named once. The composed mask is not part of it:
        // the filter phase below holds the pre-filter mask and the narrowed one at the same time.
        let served = ServedView {
            session,
            generation,
            name: view,
            data: view_data,
            segments,
            denied,
            mask_identity,
        };

        let mask = compose(
            session.satisfied(),
            &generation.overlay,
            &generation.buffer,
            base,
            &view_data.row_space,
            denied,
        );
        probe.lap(|t| &mut t.compose_ns);

        Ok(OpenView {
            served,
            mask,
            geometry,
            coordinates,
            render_scalars,
        })
    }

    /// θ's two anchors, over the session's composed mask and this view's whole row space:
    /// `V_total`, the visible cardinality, and `N_occ(zoom)`, the number of depth-`zoom` tiles
    /// holding a visible row. Both are taken from the pre-filter mask — a filtered request
    /// saturates the threshold instead, so an artifact's presence and a tile's density do not move
    /// as a viewer types — and depend only on the session's mask, view and generation, never on
    /// `bbox`, so θ does not move on a pan.
    pub(super) fn theta(
        &self,
        served: &ServedView<'_>,
        generation: &Arc<Generation>,
        geometry: &Arc<SessionGeometry>,
        mask: &EffectiveMask,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Theta {
        let v_total = mask.visible_total();
        probe.lap(|t| &mut t.theta_anchor_ns);
        let threshold = match req.filter {
            // A filtered request serves every match up to the cap. Neither anchor is consulted:
            // anchoring on the narrower matched set would make θ move as the viewer types,
            // thinning a filtered tile in proportion to the filter's selectivity instead of
            // showing its matches.
            Some(_) => Threshold::Saturated,
            None => Threshold::at_depth(
                v_total,
                self.config.theta_target_marks,
                self.occupied_tiles(
                    &served.mask_identity,
                    served.name,
                    &served.segments,
                    mask,
                    req.zoom,
                ),
            ),
        };
        probe.lap(|t| &mut t.theta_occupancy_ns);
        // Takes the ladder to `stage::BACKGROUND_DEPTH` on the pool so a zoom in and back costs
        // nothing on the way back. Only where the anchor was taken: a filtered request consults
        // neither anchor above, so it has nothing to warm.
        if req.filter.is_none() && req.zoom < crate::stage::BACKGROUND_DEPTH {
            self.spawn_ladder_fill(
                &served.mask_identity,
                served.name,
                served.session,
                generation,
                geometry,
            );
        }
        Theta { v_total, threshold }
    }

    /// This session's fragment and row projection, resolved through three rungs: a live entry, if
    /// the background refresh produced one; failing that, the one-generation-stale entry, sound
    /// only after a flush; failing that, this request builds the projection itself, or is shed
    /// with `ProjectionBuilding` if a refresh for this generation is already building one.
    pub(crate) fn session_geometry(
        &self,
        session: &Session,
        generation: &Generation,
        view: &str,
        view_data: &tessera_store::read::ViewData,
        cancel: &Option<CancelToken>,
        probe: &mut Probe,
    ) -> Result<Arc<SessionGeometry>> {
        let key = RowProjectionKey {
            token_id: session.token_id(),
            view: view.to_string(),
            segments_version: generation.segments_version,
            prefix: generation.prefix.clone(),
        };
        if let Peek::Ready(geometry) = self.row_projection_cache.peek(&key) {
            return Ok(geometry);
        }

        // Rung 2: the generation one below is the only one retention keeps.
        let space = &view_data.row_space;
        // Set on a `Ready` predecessor whether or not rung 2 can serve it: a merge leaves an
        // entry rung 2 must refuse but rung 3 still rebases from.
        let mut predecessor_resident = false;
        if let Some(previous) = key.segments_version.checked_sub(1) {
            let stale_key = RowProjectionKey {
                segments_version: previous,
                ..key.clone()
            };
            if let Peek::Ready(geometry) = self.row_projection_cache.peek(&stale_key) {
                predecessor_resident = true;
                if geometry.projection.extends_to(space) {
                    self.counters.stale_serves.fetch_add(1, Ordering::Relaxed);
                    return Ok(geometry);
                }
            }
        }

        // Rung 3: shed only if a refresh is building this request's own `segments_version`, and
        // only if rung 2 found an entry to derive from. Errs towards a duplicate build over an
        // unanswerable shed.
        if predecessor_resident
            && self.refresh_in_flight.load(Ordering::SeqCst) == key.segments_version
        {
            return Err(EngineError::ProjectionBuilding);
        }

        // A concurrent request racing the same key waits for that build and is served its result,
        // bounded by `serve.single_flight_wait_ms` and this request's cancellation token, so a
        // disconnected client releases its `ComputeGate` permit rather than holding it. Blocking
        // is safe only because this resolves on the calling thread, never a rayon worker —
        // `crate::refresh` is the caller for which that is not true, and it does not wait.
        let fragment = self.fragment_for(session, generation)?;
        probe.lap(|t| &mut t.fragment_forward_ns);
        // Constructed here, not at the top, so the allocation lands only on the path that can
        // park: every warm request returns at rung 1.
        let never_cancelled = CancelToken::new();
        let cancel = cancel.as_ref().unwrap_or(&never_cancelled);
        self.row_projection_cache
            .get_or_derive_waiting(key, None, cancel, |_source| {
                // Crosses entity space into row space over the whole fragment, taking seconds at
                // scale. This call supplies the same shared pool the tile sweep uses.
                probe.mark_projection_built();
                self.counters.full_projection_builds.fetch_add(1, Ordering::Relaxed);
                // Chosen from this principal's own grant, before any route runs; every route
                // returns the identical projection. Nothing is cached across sessions.
                let inputs = crate::projection::ProjectionInputs {
                    fragment: &fragment,
                    satisfied: session.satisfied_sorted(),
                    postings: &generation.postings,
                    deltas: &generation.delta_postings,
                    images: view_data.term_images.as_deref(),
                    force: self.projection_routes.forced(),
                };
                let projection = self
                    .pool
                    .install(|| self.projection_routes.build(&inputs, space));
                SessionGeometry {
                    fragment: Arc::clone(&fragment),
                    projection: Arc::new(projection),
                    satisfied_sorted: Arc::clone(session.satisfied_sorted()),
                    auth_data_hash: session.auth_data_hash(),
                }
            })
            .map_err(|ended| match ended {
                // A cancellation is the client's own disconnect, not backpressure: a 429 there
                // would tell an operator the server is shedding when a browser closed a tab.
                CacheWaitEnded::Budget => EngineError::ProjectionBuilding,
                CacheWaitEnded::Cancelled => EngineError::Cancelled,
            })
    }

    /// What this request's composed mask is, for the masked-count cache's key. Taken from the
    /// geometry that actually resolved, never the live generation: a session may be served a
    /// stale projection, and a key naming the wrong fragment would file one visible set's counts
    /// under another's.
    pub(crate) fn mask_identity(
        &self,
        session: &Session,
        generation: &crate::Generation,
        geometry: &crate::cache::SessionGeometry,
    ) -> crate::histogram::MaskIdentity {
        crate::histogram::MaskIdentity {
            token_id: session.token_id(),
            segments_version: generation.segments_version,
            overlay_version: generation.overlay_version,
            fragment_identity: geometry.fragment.identity,
            fragment_watermark: geometry.fragment.watermark,
        }
    }

    /// Take this session's ladder the rest of the way to [`crate::stage::BACKGROUND_DEPTH`] on the
    /// pool. Peeked here, on the request thread, before anything is spawned: once the fill has run
    /// for a session, every later request is one hash lookup and nothing else.
    pub(super) fn spawn_ladder_fill(
        &self,
        identity: &crate::histogram::MaskIdentity,
        view: &str,
        session: &Session,
        generation: &Arc<crate::Generation>,
        geometry: &Arc<crate::cache::SessionGeometry>,
    ) {
        let deepest = crate::occupancy::OccupancyKey {
            token_id: identity.token_id,
            view: view.to_string(),
            depth: crate::stage::BACKGROUND_DEPTH,
            segments_version: identity.segments_version,
            overlay_version: identity.overlay_version,
            fragment_identity: identity.fragment_identity,
            fragment_watermark: identity.fragment_watermark,
        };
        if matches!(
            self.occupancy.peek(&deepest),
            tessera_cache::Peek::Ready(_)
        ) {
            return;
        }
        self.stage.spawn(session.token_id(), || crate::stage::LadderTask {
            token_id: session.token_id(),
            view: view.to_string(),
            satisfied: session.satisfied().clone(),
            generation: Arc::clone(generation),
            geometry: Arc::clone(geometry),
        });
    }

    /// `N_occ(depth)`, θ's second anchor, for this session and view, memoised. Evaluated per
    /// requested depth, never for all seventeen: a session touches a handful of depths, and the
    /// whole ladder eagerly costs an order of magnitude more than a request needs.
    ///
    /// The mask passed in must be the unfiltered composed one: `N_occ` is an anchor, and a
    /// filtered mask reaching it would make θ move as the viewer types.
    pub(crate) fn occupied_tiles(
        &self,
        identity: &crate::histogram::MaskIdentity,
        view: &str,
        segments: &[(&SegmentData, u32)],
        mask: &crate::compose::EffectiveMask,
        depth: u8,
    ) -> u64 {
        let key = crate::occupancy::OccupancyKey {
            token_id: identity.token_id,
            view: view.to_string(),
            depth,
            segments_version: identity.segments_version,
            overlay_version: identity.overlay_version,
            fragment_identity: identity.fragment_identity,
            fragment_watermark: identity.fragment_watermark,
        };
        if let tessera_cache::Peek::Ready(hit) = self.occupancy.peek(&key) {
            return hit.0;
        }
        // The whole ladder from one walk, with every shallower rung kept too, so a session that
        // zooms out after reaching a depth pays nothing for the way back — a rung's value is the
        // same however it is reached.
        self.counters.occupancy_walks.fetch_add(1, Ordering::Relaxed);
        let ladder = crate::occupancy::occupied_tiles_ladder(mask, segments, depth);
        for rung in 0..depth {
            let mut rung_key = key.clone();
            rung_key.depth = rung;
            let _ = self.occupancy.get_or_derive(rung_key, None, |_| {
                crate::occupancy::OccupiedTiles(ladder.at(rung))
            });
        }
        match self
            .occupancy
            .get_or_derive(key, None, |_| crate::occupancy::OccupiedTiles(ladder.at(depth)))
        {
            Ok(entry) => entry.0,
            Err(tessera_cache::Building) => ladder.at(depth),
        }
    }

    /// `N_occ(depth)` for one session and view, by the request path's own route. A test hook,
    /// gated so it cannot exist in a shipped build: θ's occupied-tile anchor is otherwise
    /// observable only through mark counts, where a one-tile move is a fraction of one mark.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn occupied_tiles_for_test(&self, session: &Session, view: &str, depth: u8) -> Result<u64> {
        let generation = self.generation.load_full();
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
        let segments = segments_with_row_bases(view, view_data)?;
        let mask_identity = self.mask_identity(session, &generation, &geometry);
        Ok(self.occupied_tiles(&mask_identity, view, &segments, &mask, depth))
    }

    /// The generation a request would be answered from, and this session's composed mask over one
    /// of its views, composed by the same three calls [`Engine::viewport_stream`] makes. The
    /// generation comes back beside the mask because the segments the mask addresses are its
    /// bundle's.
    // Public for `tessera-bench`'s `identity_bands_probe`; not part of the engine's API.
    #[doc(hidden)]
    pub fn composed_mask(
        &self,
        session: &Session,
        view: &str,
    ) -> Result<(Arc<Generation>, EffectiveMask)> {
        let generation = self.generation.load_full();
        let mut probe = Probe::new();
        let mask = {
            let view_data = generation
                .bundle
                .partitions
                .values()
                .find_map(|partition| partition.views.get(view))
                .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
            let geometry =
                self.session_geometry(session, &generation, view, view_data, &None, &mut probe)?;
            let denied =
                generation
                    .denied()
                    .get(view)
                    .ok_or_else(|| EngineError::DenyMaskMissing {
                        view: view.to_string(),
                    })?;
            compose(
                session.satisfied(),
                &generation.overlay,
                &generation.buffer,
                Arc::clone(&geometry.projection),
                &view_data.row_space,
                denied,
            )
        };
        Ok((generation, mask))
    }
}
