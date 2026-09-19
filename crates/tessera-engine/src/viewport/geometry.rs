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
/// the threshold itself. `v_total` outlives the threshold because the filter's route rule reads it
/// (§8.2).
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
        // Fail closed on a view spanning partitions, for the same reason the segment guard below
        // exists: this resolves to ONE partition, and theta's anchor and every rank are then taken
        // over that partition alone — which §12.3 forbids (the anchor must be session-global, or
        // "below the cut" means different things in different partitions). The build emits one
        // partition, so this is unreachable; it is here so a §12 bundle cannot be served
        // half-masked with no error, which is the failure the multi-segment guard already refuses.
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

        // Every segment of the view, each with where its rows begin in the view's row space — see
        // [`segments_with_row_bases`] for why the pairing is keyed on `seg_id` and never positional.
        let segments = segments_with_row_bases(view, view_data)?;
        probe.lap(|t| &mut t.view_lookup_ns);

        // **Zero update-induced work on this thread, in the steady state** (decision 0044's D1).
        // Every flush advances `segments_version`, so every flush rotates this key for every live
        // session; the *measured* costs of doing anything about that here are 1.28 s for a rebuild
        // and 40.9 ms for the patch's bitmap clone alone (`probes/2026-08-04-refresh-ladder/`),
        // against a budget of 0.2 ms. Neither fits. What runs instead is a background refresh at
        // each publication (`crate::refresh`), and this is its request-side face: a three-rung
        // ladder that builds nothing a refresh is about to produce.
        //
        // **Guardrail (D-D/D-F): nothing reachable from a rayon worker below may touch this
        // cache.** The value is resolved once, here, on the calling thread, strictly before the
        // parallel tile sweep begins, and is then only *borrowed* (via `compose`'s
        // `EffectiveMask`) by every `tile_sweep` call — never re-fetched or re-built per tile.
        let geometry = self.session_geometry(session, generation, view, view_data, cancel, probe)?;
        // Minted here, from the geometry that actually resolved — see `view_coordinates`.
        let coordinates = self.view_coordinates(generation, &geometry, view);
        // The same rule one structure along: the masked-count cache's key names the fragment this
        // request composes against, which under stale-serve is the entry's and not the newest one.
        let mask_identity = self.mask_identity(session, generation, &geometry);
        let base = Arc::clone(&geometry.projection);
        probe.lap(|t| &mut t.row_projection_ns);

        // **Render columns only, narrowed once — for the head and the gather alike.**
        // `declared_scalars` is the compiled schema and includes `filter`-only and blob-resident
        // columns, which are entity-space and absent from `columns.arrow` by design; taking the
        // full list would publish a column of nulls under a name a client can see, and — because
        // the head's names are zipped positionally with the gathered buffers — caption a render
        // column's values with a non-render column's name wherever the two lists diverge. One
        // construction site is what keeps the names and the buffers the same list.
        let mut render_scalars: Vec<_> = generation
            .bundle
            .manifest
            .render_scalars()
            .cloned()
            .collect();
        // **Then this view's scoped render columns, and only this view's** (`views.md` §5). A
        // group-scoped attribute declaring `render` occupies a slot in the row tail of every view
        // of its group — and of any group sharing those views — and in no other, so the list is
        // per view where the bundle-wide half above is not. Appended rather than interleaved,
        // which is the order the build and every flush write the lanes in.
        render_scalars.extend(scoped_render_scalars(
            &generation.bundle.manifest,
            view,
            session.visible_views(),
        ));

        // D-C checkpoint: before compose, one of the two long serial-prefix stages this task
        // guards. Placed after the (non-cancellable, D-G) row-projection build so a cancellation
        // observed here never interrupts that build — only work this request would otherwise go
        // on to do itself.
        check_cancelled(cancel)?;

        // **Fail-closed on a missing entry.** Every view the bundle carries has one, empty when
        // nothing is denied (`compose::derive_denied`), so an absent key means the mask and the
        // bundle disagree about what this generation holds. Serving that as "nothing is denied
        // here" would publish suppressed and deleted rows on the map with no error anywhere —
        // the same shape as `SegmentWithoutRowBase`, and refused the same way.
        let denied = generation
            .denied()
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;

        // Everything this request's view fixes, named once — see [`ServedView`]. The composed mask
        // is not part of it: the filter below holds the pre-filter mask and the narrowed one at the
        // same time.
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

    /// θ's two anchors, both over the session's **composed** mask and this view's whole row
    /// space: `V_total`, the visible cardinality, and `N_occ(zoom)`, the number of depth-`zoom`
    /// tiles holding at least one visible row. Both must be the composed figures and not
    /// `base`'s — see `Threshold::at_depth`'s doc for the I2 argument and the concrete channel
    /// the pre-overlay figures open.
    ///
    /// Both are viewport-*invariant*: they depend on the session's mask, the view and the
    /// generation, never on `bbox`, so θ does not move when the viewer pans — which is the churn
    /// §7.2 forbids. `N_occ` is a function of the depth, so θ moves on a *zoom*, which is what
    /// makes the mean occupied tile draw `m_target` marks at every depth; nesting survives it
    /// because `N_occ` is non-decreasing in depth, so θ is monotone (§7.2). Both move on an
    /// overlay swap, which is accepted: swaps are rare against pans, and because the served set
    /// is a `tessera_id` prefix, a small θ move perturbs only the marks nearest the cut.
    ///
    /// **The mask is the pre-filter one**, which is what the caller holds at this point: a filtered
    /// request saturates the threshold rather than re-anchoring it (**I12**).
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
            // §8.5's match-layer count rule: a filtered request serves every match, up to the cap.
            // The θ threshold clause does not thin a filtered selection, and saturating is how the
            // definition says "in full": `C_θ = |vis(T)|`, so `served = min(matched, cap)` per
            // tile, with the cap-many smallest `tessera_id`s when a tile is over — the same prefix
            // rule as ever, so nesting across zooms is untouched. Neither anchor is walked, both
            // being inputs to a threshold this request does not consult.
            //
            // This is NOT a re-anchor. θ's anchors stay the unfiltered composed mask's, and §5.2 of
            // `filter-surface.md` forbids anchoring on `M_sel` (a threshold that moved as the
            // viewer typed). The rule here is the other half of the same section: the anchor never
            // narrows, and the match layer never samples. Before this, a filtered tile was pushed
            // through the unfiltered θ odds — a tile narrowed from 4,000 visible to 40 matched drew
            // ~1% of 40, i.e. the k_min floor — so the map thinned in proportion to the filter's
            // selectivity instead of showing the matches.
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
        // **The rest of the ladder, on the pool, after this request has its own rung** — see
        // `crate::stage`. This request walked at `zoom` and kept every rung below it; the fill
        // takes the ladder the rest of the way to `stage::BACKGROUND_DEPTH`, so a session's zoom
        // to depth 12 and its zoom back out cost nothing. Spawned here rather than at authorise
        // because `N_occ` is counted over the composed mask, which needs the row projection this
        // request has just resolved and which no session has before its first request.
        //
        // **Only where the anchor was taken.** A filtered request saturates θ and consults neither
        // anchor (above), so it has no reason to warm one; the next unfiltered request spawns the
        // fill for itself.
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

        // Rung 2. The generation exactly one below is the only one the retention depth keeps
        // (`crate::cache::KEEP_SUPERSEDED_GENERATIONS`) and the only one an append can be served
        // across.
        let space = &view_data.row_space;
        // Whether the refresh pass has an entry to derive this request's key from. Set on a
        // `Ready` predecessor whether or not rung 2 can serve it: a merge leaves an entry rung 2
        // must refuse, and the pass still rebases it into this key.
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

        // Rung 3. **Compared against this request's own generation**, not read as a boolean: the
        // claim names the `segments_version` whose refresh is running, so a pass still finishing
        // for a *superseded* generation does not shed a request whose key nothing is coming to
        // produce — which would be a 429 with no end.
        //
        // **And conjoined with rung 2's peek, for the same reason.** The shed trades a build for a
        // wait, so it is owed only where the wait ends in the value. `crate::refresh` iterates
        // resident entries and derives each successor from the entry it already holds, so the only
        // keys a pass produces are the successors of what was resident when it started. A session
        // authorised after the publication has no resident entry, no pass will reach its key, and
        // refusing it lasts the whole pass and buys it nothing. The peek above is exact because
        // the retention depth keeps one superseded generation
        // (`crate::cache::KEEP_SUPERSEDED_GENERATIONS`), so the version immediately below is the
        // only predecessor the cache can hold.
        //
        // A predecessor under a superseded prefix is not seen here, because the peek carries this
        // generation's prefix. The pass rebuilds such an entry (`crate::refresh::Carry::Rebuild`),
        // so that request builds where it could have been shed. That is the direction this rung
        // may err in: a build that duplicates a pass's work costs the requester the rebuild, where
        // a shed no pass will answer costs it the request.
        if predecessor_resident
            && self.refresh_in_flight.load(Ordering::SeqCst) == key.segments_version
        {
            return Err(EngineError::ProjectionBuilding);
        }

        // D-G slot-state single-flight (F4, `tessera-bench/src/arms/load.rs:34-76`): the map lock
        // is held only for the O(1) `Building`/`Ready` transition — never across the build — so
        // distinct sessions' first viewports do not serialise behind one global lock.
        //
        // **A concurrent request racing the *same* key waits for that build and is served its
        // result** (decision 0058). It used to be refused, which reached the client as a 429 whose
        // `Retry-After` was shorter than the build it was waiting for — so at 10⁹ a client racing
        // itself exhausted its retries before work that was always going to succeed finished, and
        // showed a blank map. The wait is bounded by `serve.single_flight_wait_ms` and observes
        // this request's cancellation token, so a disconnected client releases its
        // `ComputeGate` permit rather than holding it to the budget.
        //
        // Blocking here is safe because of the guardrail below: this resolves on the **calling**
        // thread, never a rayon worker. `crate::refresh` is the caller for which that is not true,
        // and it does not wait.
        //
        // **Guardrail (D-D/D-F): nothing reachable from a rayon worker below may touch this
        // cache.** The value is resolved once, here, on the calling thread, strictly before the
        // parallel tile sweep begins, and is then only *borrowed* by every `tile_sweep` call.
        let fragment = self.fragment_for(session, generation)?;
        probe.lap(|t| &mut t.fragment_forward_ns);
        // Constructed here rather than at the top of this function so the allocation lands only on
        // the path that can actually park — every warm request returns at rung 1 above. An
        // embedder that supplies no token (every non-server caller of this API) waits on one that
        // is never flipped, which is the same posture `check_cancelled` takes for `None`.
        let never_cancelled = CancelToken::new();
        let cancel = cancel.as_ref().unwrap_or(&never_cancelled);
        self.row_projection_cache
            .get_or_derive_waiting(key, None, cancel, |_source| {
                // Crosses entity space into row space over the *whole* fragment
                // (`Permutation::project`'s cost note: seconds at 10⁹ rows). `Permutation::project`
                // parallelises internally but owns no pool of its own — this is the one call site
                // that supplies one, the same shared pool the tile sweep uses (D-D: no second,
                // per-request pool).
                probe.mark_projection_built();
                self.counters.full_projection_builds.fetch_add(1, Ordering::Relaxed);
                // **The route is chosen here, from this principal's own grant, before any route
                // runs**, and every route returns the identical projection
                // (`crate::projection::RowProjection::new`). The images are the bundle's, mapped;
                // nothing is cached across sessions.
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
                    satisfied_at: session.segments_version_at_authorise(),
                }
            })
            .map_err(|ended| match ended {
                // The budget expiry is what `SingleFlightBackpressure` now means: the request
                // waited for a build and it did not arrive. A cancellation is the client's own
                // disconnect and must not be dressed up as backpressure — reporting it as a 429
                // would tell an operator the server is shedding when a browser closed a tab.
                CacheWaitEnded::Budget => EngineError::ProjectionBuilding,
                CacheWaitEnded::Cancelled => EngineError::Cancelled,
            })
    }

    /// What this request's composed mask is, for the masked-count cache's key.
    ///
    /// **Taken from the geometry that actually resolved**, never from the live generation's idea of
    /// it: a session may be served a one-generation-stale projection (decision 0044), so the
    /// fragment a request composes against is the entry's and not the newest one there is. A key
    /// naming the wrong fragment would file one visible set's counts under another's.
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

    /// Take this session's ladder the rest of the way to [`crate::stage::BACKGROUND_DEPTH`], on
    /// the pool — see [`crate::stage`] for what that buys and where it stops.
    ///
    /// **The memo is peeked here, on the request thread, before anything is spawned.** In the
    /// steady state — every request after the fill has run — this is one hash lookup and nothing
    /// else: no pool task, no lock, no clone of a term set. The fill peeks the same key again for
    /// itself, because between this peek and its own the memo can only have gained entries.
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
            crate::single_flight::Peek::Ready(_)
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

    /// `N_occ(depth)` for this session and view — θ's second anchor (§7.2), memoised.
    ///
    /// **Evaluated per requested depth, never for all seventeen.** A session touches a handful of
    /// depths, and the walk is `O((runs + N_occ(d)) · log)` per depth against the mask and the
    /// Morton column; computing the whole ladder eagerly costs an order of magnitude more than a
    /// request needs (`crate::occupancy`'s module doc carries the figures).
    ///
    /// The memo's key is [`crate::occupancy::OccupancyKey`], which is the masked-count cache's key
    /// terms plus the view and the depth: every one of them is a reason the composed mask or the
    /// row space moved. A key that is being built by another caller is answered by building here
    /// and not retaining, exactly as a region decomposition whose wait ran out is: the key fixes
    /// the mask, the view and the depth, so two builders reach the same number and the waste is one
    /// walk rather than a wrong answer.
    ///
    /// **The mask must be the unfiltered composed one.** `N_occ` is an anchor, so a filtered mask
    /// reaching it would make θ a function of what the viewer typed (**I12**); the call site takes
    /// both anchors before the filter is evaluated.
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
        if let crate::single_flight::Peek::Ready(hit) = self.occupancy.peek(&key) {
            return hit.0;
        }
        // The whole ladder from one walk, and every rung below this one memoised on the way.
        // `crate::occupancy::occupied_tiles_ladder` explains why the shallower rungs are free once
        // the deepest has been walked; what this loop adds is that they are *kept*, so a session
        // that reaches a depth and then zooms out pays nothing for the way back.
        //
        // **Each rung's value is the same however it was reached.** The depth-`d'` sketch is a
        // function of the depth-`d'` occupied tile set alone, and the ancestors of the depth-`d`
        // tiles are exactly that set, so a rung filled by a walk at 16 and the same rung filled by
        // a walk at 6 hold identical registers and answer identically. Without that the memo would
        // be answering from whichever depth happened to be requested first.
        // **This makes the memo up to seventeen entries per `(session, view, generation)` where it
        // was one per depth actually visited.** The value is a `u64` and the cost is the cache's
        // per-entry floor over a key holding a view name, so the byte bound absorbs it; what it
        // buys is that the deepest walk a session makes is the only one it makes.
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
            Err(crate::single_flight::Building) => ladder.at(depth),
        }
    }

    /// `N_occ(depth)` for one session and view, by the request path's own route.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Engine::set_background_refresh_for_test`]'s argument. θ's occupied-tile anchor is
    /// otherwise observable only through mark counts, where a one-tile move is a fraction of one
    /// mark — so the I2 property that it is the **composed** figure, and not the cached row
    /// projection's, has no other surface a test can assert on. Composing a second mask in the test
    /// instead would assert against that transcription rather than against this one.
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
    /// of its views.
    ///
    /// Composed here by the same three calls [`Engine::viewport_stream`] makes — the session's
    /// geometry, this generation's overlay and buffer, this view's deny mask — so that a caller
    /// evaluating selection a second way evaluates it over the served input rather than over a
    /// transcription of the composition rule. The generation comes back beside the mask because
    /// the segments the mask addresses are its bundle's.
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
