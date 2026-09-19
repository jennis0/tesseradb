//! The session's geometry for a view: the projection ladder, the composed mask and occupancy.

use super::*;

impl Engine {
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
