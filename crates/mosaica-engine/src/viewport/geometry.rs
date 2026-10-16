//! The session's geometry for a view: the projection ladder, the composed mask and occupancy.

use super::*;

/// What a request resolves before it counts anything: the view it is served from, this session's
/// geometry over it in every shard, the composed mask and the columns the response renders.
pub(crate) struct OpenView<'a> {
    pub(crate) served: ServedView<'a>,
    /// The composed mask, before any filter narrows it.
    pub(crate) mask: ShardedMask,
    /// The generation every side's geometry was taken for: one behind `generation` while a
    /// refresh has not yet replaced the grant's projections.
    pub(super) stamp: GenerationStamp,
    pub(crate) coordinates: ViewCoordinates,
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

/// A session's geometry in every shard holding a view, from one generation.
pub(crate) struct Geometries<'g> {
    /// Each shard holding the view, its data for the view and the geometry, ascending by shard.
    pub(crate) sides: Vec<(&'g crate::Shard, &'g ViewData, Arc<SessionGeometry>)>,
    /// The generation every side's geometry was taken for.
    pub(crate) segments_version: u64,
}

/// Refuses a view no shard holds, and one held in more than one partition: θ's anchor and every
/// rank must be session-global. The second is unreachable while the build emits one partition;
/// kept so a bundle spanning more is not served half-masked with no error.
pub(crate) fn check_view(generation: &Generation, view: &str) -> Result<()> {
    let carriers = generation
        .bundle
        .partitions
        .values()
        .filter(|partition| {
            partition
                .shards
                .iter()
                .any(|shard| shard.views.contains_key(view))
        })
        .count();
    match carriers {
        0 => Err(EngineError::UnknownView(view.to_string())),
        1 => Ok(()),
        _ => Err(EngineError::MultiPartitionView(view.to_string())),
    }
}

impl Engine {
    /// Resolve the view this request is served from, and everything fixed by that resolution.
    pub(crate) fn open_view<'a>(
        &self,
        session: &'a Session,
        generation: &'a Generation,
        view: &'a str,
        cancel: &Option<CancelToken>,
        probe: &mut Probe,
    ) -> Result<OpenView<'a>> {
        check_view(generation, view)?;
        probe.lap(|t| &mut t.view_lookup_ns);

        // Every flush rotates this cache key for every live session; rebuilding it here would
        // cost on the order of a second, far above a request's budget. A background refresh
        // builds it instead; `session_geometries`'s three-rung ladder finds what that produced.
        //
        // Resolved once, before the parallel tile sweep begins, and only borrowed after by every
        // `tile_sweep` call. Nothing reachable from a rayon worker may touch this cache: a worker
        // blocking on a build only the calling thread can drive would deadlock the pool.
        let geometries = self.session_geometries(session, generation, view, cancel, probe)?;
        let projection_segments_version = geometries.segments_version;
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

        let mut sides = Vec::with_capacity(geometries.sides.len());
        for (shard, data, geometry) in geometries.sides {
            // Named from the geometry that actually resolved, never the live generation: a
            // session may be served stale projections, and a key naming the wrong fragment would
            // file one visible set's counts under another's.
            let versions = SideVersions {
                shard: shard.id(),
                overlay_version: shard.overlay_version,
                fragment_identity: geometry.fragment.identity,
                fragment_watermark: geometry.fragment.watermark,
            };
            sides.push(ServedSide {
                shard,
                data,
                segments: segments_with_row_bases(view, data)?,
                denied: shard.denied_in(view)?,
                geometry,
                versions,
            });
        }
        let versions: Vec<SideVersions> = sides.iter().map(|side| side.versions).collect();
        let coordinates = self.view_coordinates(session, generation, &versions, view);
        let mask_identity = MaskIdentity::of(
            session.token_id(),
            session.terms_digest(),
            generation.segments_version,
            projection_segments_version,
            &versions,
        );

        let mask = ShardedMask::new(sides.iter().map(|side| {
            let leaf = side.shard.compose(
                session.satisfied(),
                Arc::clone(&side.geometry.projection),
                view,
                &side.data.row_space,
                side.denied,
            );
            (side.shard.id(), leaf)
        }));
        probe.lap(|t| &mut t.compose_ns);

        // Everything this request's view fixes, named once. The composed mask is not part of it:
        // the filter phase below holds the pre-filter mask and the narrowed one at the same time.
        let served = ServedView {
            session,
            generation,
            name: view,
            sides,
            mask_identity,
            cancel: cancel.clone(),
            turn: Default::default(),
        };

        Ok(OpenView {
            served,
            mask,
            stamp: GenerationStamp {
                prefix: generation.prefix.clone(),
                segments_version: projection_segments_version,
            },
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
        mask: &ShardedMask,
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
                    &served.sides,
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
                &served.sides,
            );
        }
        Theta { v_total, threshold }
    }

    /// This session's grant's fragment and row projection in every shard holding `view`, all from
    /// one generation, by three rungs: every live entry; else every shard's one-generation-stale
    /// entry, after a flush and only for a session already served this view at no later
    /// generation; else the missing live entries from [`Self::derive_geometry`], or
    /// `ProjectionBuilding` where the session was served the view and a refresh is building them.
    /// A response never combines a stale shard with a live one.
    pub(crate) fn session_geometries<'g>(
        &self,
        session: &Session,
        generation: &'g Generation,
        view: &str,
        cancel: &Option<CancelToken>,
        probe: &mut Probe,
    ) -> Result<Geometries<'g>> {
        let shards: Vec<(&crate::Shard, &ViewData)> = generation
            .shards()
            .filter_map(|shard| Some((shard, shard.data.views.get(view)?)))
            .collect();
        if shards.is_empty() {
            return Err(EngineError::UnknownView(view.to_string()));
        }
        let keys: Vec<RowProjectionKey> = shards
            .iter()
            .map(|(shard, _)| RowProjectionKey {
                grant: session.terms_digest(),
                view: view.to_string(),
                shard: shard.id(),
                identity: shard.identity,
                segments_version: shard.segments_version,
                prefix: generation.prefix.clone(),
            })
            .collect();
        // Each shard's row space the geometries address, recorded against the session.
        let served = |geometries: Vec<Arc<SessionGeometry>>,
                      keys: &[RowProjectionKey],
                      segments_version: u64| {
            for ((shard, _), key) in shards.iter().zip(keys) {
                session.record_served(view, shard.id(), key.segments_version);
            }
            Geometries {
                sides: shards
                    .iter()
                    .zip(geometries)
                    .map(|(&(shard, data), geometry)| (shard, data, geometry))
                    .collect(),
                segments_version,
            }
        };
        let ready = |key: &RowProjectionKey| match self.row_projection_cache.peek(key) {
            Peek::Ready(geometry) => Some(geometry),
            _ => None,
        };
        let live: Vec<Option<Arc<SessionGeometry>>> = keys.iter().map(ready).collect();
        if live.iter().all(Option::is_some) {
            return Ok(served(
                live.into_iter().flatten().collect(),
                &keys,
                generation.segments_version,
            ));
        }

        // Rung 2: the generation one below is the only one retention keeps, each shard's row
        // space as it stood there.
        // A `Ready` predecessor rung 2 does not serve: rung 3 sheds on it, or carries it forward.
        let mut predecessors: Vec<Option<RowProjectionKey>> = vec![None; shards.len()];
        if let Some(previous) = generation.segments_version.checked_sub(1) {
            let mut stale = Vec::with_capacity(shards.len());
            for (index, ((shard, view_data), key)) in shards.iter().zip(&keys).enumerate() {
                let Some(before) = generation.segments_version_before(shard) else {
                    continue;
                };
                let stale_key = RowProjectionKey {
                    segments_version: before,
                    ..key.clone()
                };
                let Some(geometry) = ready(&stale_key) else {
                    continue;
                };
                if session
                    .served_at(view, shard.id())
                    .is_some_and(|at| at <= before)
                    && geometry.projection.extends_to(&view_data.row_space)
                {
                    stale.push(geometry);
                }
                predecessors[index] = Some(stale_key);
            }
            if stale.len() == shards.len() {
                self.counters.stale_serves.fetch_add(1, Ordering::Relaxed);
                let stale_keys: Vec<RowProjectionKey> =
                    predecessors.iter().flatten().cloned().collect();
                return Ok(served(stale, &stale_keys, previous));
            }
        }

        // Rung 3. Errs towards a duplicate build over an unanswerable shed.
        let sheds =
            shards
                .iter()
                .zip(&live)
                .zip(&predecessors)
                .any(|(((shard, _), live), predecessor)| {
                    live.is_none()
                        && predecessor.is_some()
                        && session.served_at(view, shard.id()).is_some()
                });
        if sheds && self.refresh_in_flight.load(Ordering::SeqCst) == generation.segments_version {
            return Err(EngineError::ProjectionBuilding);
        }

        let mut geometries = Vec::with_capacity(shards.len());
        for (((shard, view_data), key), (live, predecessor)) in shards
            .iter()
            .zip(keys.iter().cloned())
            .zip(live.into_iter().zip(predecessors))
        {
            let geometry = match live {
                Some(geometry) => geometry,
                None => self.derive_geometry(
                    session,
                    shard,
                    view_data,
                    key,
                    predecessor.as_ref(),
                    cancel,
                    probe,
                )?,
            };
            geometries.push(geometry);
        }
        Ok(served(geometries, &keys, generation.segments_version))
    }

    /// Rung 3 in one shard: the live entry under `key`, carried forward from `predecessor` where a
    /// flush or a merge allows and built otherwise.
    ///
    /// A concurrent request racing the same key waits for that build and is served its result,
    /// bounded by `serve.single_flight_wait_ms` and this request's cancellation token, so a
    /// disconnected client releases its `ComputeGate` permit rather than holding it. Blocking is
    /// safe only because this resolves on the calling thread, never a rayon worker —
    /// `crate::refresh` is the caller for which that is not true, and it does not wait.
    #[allow(clippy::too_many_arguments)]
    fn derive_geometry(
        &self,
        session: &Session,
        shard: &crate::Shard,
        view_data: &ViewData,
        key: RowProjectionKey,
        predecessor: Option<&RowProjectionKey>,
        cancel: &Option<CancelToken>,
        probe: &mut Probe,
    ) -> Result<Arc<SessionGeometry>> {
        let space = &view_data.row_space;
        let fragment = self.fragment_for(session, shard, None)?;
        probe.lap(|t| &mut t.fragment_ns);
        // Constructed here, not at the top, so the allocation lands only on the path that can
        // park: every warm request returns at rung 1.
        let never_cancelled = CancelToken::new();
        let cancel = cancel.as_ref().unwrap_or(&never_cancelled);
        self.row_projection_cache
            .get_or_derive_waiting(key, predecessor, cancel, |source| {
                let carried = source.and_then(|source| {
                    self.pool
                        .install(|| {
                            crate::refresh::carried(source, &fragment, &shard.delta_postings, space)
                        })
                });
                let projection = carried.unwrap_or_else(|| {
                    // Crosses entity space into row space over the whole fragment, taking seconds
                    // at scale. This call supplies the same shared pool the tile sweep uses.
                    probe.mark_projection_built();
                    self.counters.full_projection_builds.fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "fault-injection")]
                    self.switches.hold_projection_build_if_wanted();
                    // Chosen from the grant, before any route runs; every route returns the
                    // identical projection, which every session with the grant then reads.
                    let inputs = crate::projection::ProjectionInputs {
                        fragment: &fragment,
                        satisfied: session.grant().keys(),
                        postings: &shard.postings,
                        deltas: &shard.delta_postings,
                        images: view_data.term_images.as_deref(),
                        force: self.projection_routes.forced(),
                    };
                    self.pool
                        .install(|| self.projection_routes.build(&inputs, space))
                });
                SessionGeometry {
                    fragment: Arc::clone(&fragment),
                    projection: Arc::new(projection),
                    grant: session.grant().clone(),
                }
            })
            .map_err(|ended| match ended {
                // A cancellation is the client's own disconnect, not backpressure: a 429 there
                // would tell an operator the server is shedding when a browser closed a tab.
                CacheWaitEnded::Budget => EngineError::ProjectionBuilding,
                CacheWaitEnded::Cancelled => EngineError::Cancelled,
            })
    }

    /// Take this session's ladder the rest of the way to [`crate::stage::BACKGROUND_DEPTH`] on the
    /// pool. Peeked here, on the request thread, before anything is spawned: once the fill has run
    /// for a session, every later request is one hash lookup and nothing else.
    pub(super) fn spawn_ladder_fill(
        &self,
        identity: &crate::figures::MaskIdentity,
        view: &str,
        session: &Session,
        generation: &Arc<crate::Generation>,
        sides: &[ServedSide<'_>],
    ) {
        let depth = crate::stage::BACKGROUND_DEPTH;
        let deepest = crate::occupancy::OccupancyKey::of(identity, view, depth);
        if matches!(
            self.occupancy.peek(&deepest),
            mosaica_cache::Peek::Ready(_)
        ) {
            return;
        }
        self.stage.spawn(session.token_id(), || crate::stage::LadderTask {
            identity: *identity,
            view: view.to_string(),
            satisfied: session.satisfied().clone(),
            generation: Arc::clone(generation),
            geometries: sides
                .iter()
                .map(|side| (side.shard.id(), Arc::clone(&side.geometry)))
                .collect(),
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
        identity: &crate::figures::MaskIdentity,
        view: &str,
        sides: &[ServedSide<'_>],
        mask: &ShardedMask,
        depth: u8,
    ) -> u64 {
        let key = crate::occupancy::OccupancyKey::of(identity, view, depth);
        if let mosaica_cache::Peek::Ready(hit) = self.occupancy.peek(&key) {
            return hit.0;
        }
        // The whole ladder from one walk, with every shallower rung kept too, so a session that
        // zooms out after reaching a depth pays nothing for the way back — a rung's value is the
        // same however it is reached.
        self.counters.occupancy_walks.fetch_add(1, Ordering::Relaxed);
        let parts = crate::occupancy::occupancy_parts(
            mask,
            sides.iter().map(|side| (side.shard.id(), &side.segments[..])),
        );
        let ladder = crate::occupancy::occupied_tiles_ladder(&parts, depth);
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
            Err(mosaica_cache::Building) => ladder.at(depth),
        }
    }

    /// `N_occ(depth)` for one session and view, by the request path's own route. A test hook,
    /// gated so it cannot exist in a shipped build: θ's occupied-tile anchor is otherwise
    /// observable only through mark counts, where a one-tile move is a fraction of one mark.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn occupied_tiles_for_test(&self, session: &Session, view: &str, depth: u8) -> Result<u64> {
        let generation = self.generation.load_full();
        let mut probe = Probe::new();
        let open = self.open_view(session, &generation, view, &None, &mut probe)?;
        let served = &open.served;
        Ok(self.occupied_tiles(
            &served.mask_identity,
            view,
            &served.sides,
            &open.mask,
            depth,
        ))
    }

    /// The generation a request would be answered from, and this session's composed mask over one
    /// of its views in `shard`, composed as [`Engine::viewport_stream`] composes it. The
    /// generation comes back beside the mask because the segments the mask addresses are its
    /// bundle's.
    // Public for `mosaica-bench`'s `identity_bands_probe`; not part of the engine's API.
    #[doc(hidden)]
    pub fn composed_mask(
        &self,
        session: &Session,
        view: &str,
        shard: ShardId,
    ) -> Result<(Arc<Generation>, EffectiveMask)> {
        let generation = self.generation.load_full();
        let mut probe = Probe::new();
        let mask = self
            .open_view(session, &generation, view, &None, &mut probe)?
            .mask
            .into_leaf(shard)
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
        Ok((generation, mask))
    }
}
