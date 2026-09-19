//! Session authorisation.
//!
//! [`Engine::authorise`] turns a credential into a [`Session`]: the plugin's granted descriptors
//! are resolved against the bundle dictionary (an unknown descriptor is simply unsatisfied, never
//! an error — the dictionary is the authority on which descriptors exist), and the resulting term
//! set is unioned into a mask fragment via [`FragmentCache`] — this union *is* the authorisation
//! decision (I2).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use tessera_authz::{FragmentCacheError, FrozenFragment};
use tessera_types::TermId;

#[cfg(doc)]
use tessera_authz::FragmentCache;

use crate::engine::{hex_encode, Engine};
use crate::error::{EngineError, Result};
use crate::Generation;

/// One authorised viewer session: the credential's granted term set and the mask fragment it
/// unions to (I2 — the fragment *is* the authorisation decision, computed once here and reused,
/// never recomputed per viewport).
///
/// `handles` (a per-session entity-ID → wire-`Handle` table) is deliberately absent: that state
/// is owned by `tessera-wire`, which must not gain a dependency on this crate's
/// `EntityId` (I10) any more than this crate should depend on `tessera-wire`. `tessera-server`
/// holds a session's handle table alongside, not inside, this struct.
pub struct Session {
    /// Bearer token: 32 random bytes, hex-encoded.
    pub token: String,
    /// A process-local identity for this session, distinct from `token` — used as (part of) the
    /// row-projection cache key (`(token_id, view, segments_version)`, shared-context
    /// constraint 8) so the cache never has to hash or compare the full token string.
    pub token_id: u64,
    /// The credential's granted terms, resolved to bundle-relative `TermId`s. An unknown
    /// descriptor (no dictionary entry) is simply absent here — never an error.
    ///
    /// **Resolved once, at authorise, and never re-resolved in place** — see [`Session::is_stale`],
    /// whose third rule this is. A flush that promotes one of the descriptors that dropped out
    /// leaves this set as it was; the remedy is a new session, not a mutation of this one.
    pub satisfied: FxHashSet<TermId>,
    /// The materialised mask fragment (I2): the union of every satisfied term's postings, over
    /// the base and every delta tier live at the moment this session authorised.
    ///
    /// **It goes stale, and [`Engine::fragment_for`] is what brings it forward.** A flush publishes
    /// a delta tier and moves the generation's watermark, and composition treats entities *below*
    /// the watermark as fragment-resident — so a flushed entity is in neither this fragment nor the
    /// ingest buffer until the fragment is rebuilt at the new watermark. Nothing on the request
    /// path may read this field directly for that reason; see `Engine::fragment_for`.
    pub fragment: Arc<FrozenFragment>,
    /// `satisfied`, sorted — the [`tessera_authz::FragmentCache`] key component, kept rather than
    /// re-sorted per request so bringing the fragment forward costs no allocation on a hit.
    /// `Arc` so the row-projection cache's entry can carry it for the background refresh, which
    /// has no session registry to look it up in — see [`crate::cache::SessionGeometry`].
    pub(crate) satisfied_sorted: Arc<Vec<TermId>>,
    /// **The descriptor this session's credential presented for each satisfied term** — the one
    /// route by which a term ordinal becomes a string a viewer is shown (decision 0114).
    ///
    /// The drill-down's `labels` array is built from this map alone: an entity's own term list is
    /// intersected with it, and a term the map does not hold has no name here and cannot be
    /// served. That is the satisfied-only rule expressed as a data structure rather than as a
    /// filter — a bug in the intersection can lose a label the viewer holds, and cannot invent one
    /// they do not.
    ///
    /// **This is also why the bundle carries no reverse dictionary.** Resolving an ordinal to its
    /// descriptor globally would need an index over every term the corpus knows — at the plugin's
    /// declared 2×10⁸ terms, gigabytes of it — for a surface that may only ever name terms the
    /// caller already handed in. The credential's own descriptors are bounded by
    /// `max_terms_per_token` and are already in hand at authorise.
    ///
    /// `public` is here with a descriptor no credential supplied, exactly as it is in
    /// [`Session::satisfied`] and for the same reason: it is the label every principal holds.
    pub(crate) satisfied_descriptors: Arc<FxHashMap<TermId, Vec<u8>>>,
    /// **The visible-view set** (`views.md` §6): every view of every group this principal may
    /// reach, resolved once here at authorise and **fixed for this session's life**.
    ///
    /// Fixed is a guarantee rather than an oversight. Every view is evaluated at authorise
    /// whatever the outcome, so the request-time check is one set-membership lookup and a
    /// gate-failed name costs the same work as a name nobody declared — r23's
    /// work-indistinguishability standard, and the closure Appendix C's C4 records for
    /// `/v1/items`. A view **created after** this session authorised is therefore a 404 to it
    /// until it re-authorises (owner ruling 2026-08-30): creation is rare, tokens expire, and the
    /// alternatives — a per-request gate evaluation, or a lazily-evaluated miss — each cost
    /// exactly the property this field exists to hold. Roster immutability (`views.md` §3.2) is
    /// the other half: a gate, once written, never changes, so a fixed set can never hold a stale
    /// *widening*.
    ///
    /// `Arc` because every request path reads it and none of them may clone the set.
    pub visible_views: Arc<crate::gate::VisibleViews>,
    /// `sha256(auth_data)` — the cache's caller obligation, kept for the same reason.
    ///
    /// A digest of the credential, never the credential: this lives for the session's lifetime in
    /// a struct the server holds per connection, and the bearer secret must not.
    pub(crate) auth_data_hash: [u8; 32],
    /// Unix timestamp (seconds) after which this session is no longer valid.
    pub expires_at: u64,
    /// How many of the credential's granted descriptors had **no dictionary entry** at authorise,
    /// and the dictionary length they were resolved against — together, §3.3's staleness
    /// condition. See [`Session::is_stale`].
    ///
    /// Deliberately not `pub`. The boolean is the whole of what §3.3 specifies; the count is a
    /// fact about how many of this viewer's descriptors the corpus does not carry, which is
    /// strictly more than the boolean and would need its own leak-register argument before
    /// anything could put it on the wire.
    pub(crate) unresolved_count: usize,
    /// See [`Self::unresolved_count`].
    pub(crate) dict_len_at_authorise: u32,
    /// **The generation this session's `satisfied` was resolved against**, which is what
    /// [`Engine::fragment_for`] must hand `FragmentCache::get_or_build` alongside the frozen term
    /// set (#112).
    ///
    /// Distinct from [`Self::dict_len_at_authorise`] and not a duplicate of it: that one answers
    /// *has the dictionary grown since* and is the staleness condition's own input, where this one
    /// identifies the resolution. A length is a faithful stand-in for a generation only while the
    /// dictionary is append-only, which a fold breaks — `get_or_build`'s doc argues the difference,
    /// and it is why keying the memo on the length left a trap for a change nobody had made yet.
    pub(crate) segments_version_at_authorise: u64,
}

impl Session {
    /// **§3.3 — is this session's mask behind the corpus?** True iff this credential named a
    /// descriptor the dictionary did not carry at authorise *and* the dictionary has grown since.
    ///
    /// Two loads and a branch, evaluated lazily by whoever asks — never swept. Nothing walks the
    /// session registry when a flush promotes a term, which keeps the executor free of an
    /// O(sessions) publication step (decision 0035). `Dict` is generation-scoped (§3.2), so the
    /// current length comes off the generation the caller has already loaded once at request start
    /// (lifecycle §1.1's ordering invariant). Decision 0020 is untouched: a count and an integer
    /// are not authorisation data.
    ///
    /// **Over-reports in one direction, and that is the safe one.** A session with one unresolved
    /// descriptor is hinted whenever *any* term is promoted, not only its own; the false direction
    /// costs one voluntary re-authorisation. A session with nothing unresolved is never hinted.
    /// (The refinement — comparing digests of the unresolved descriptors against the promoted ones
    /// — is deliberately not built: it retains more and leaks more, confirming that *their*
    /// descriptor now exists where this says only that some term appeared.)
    ///
    /// **⊘ Specified, not implemented: the wire representation.** §3.3 states the internal
    /// condition only. No response carries this, and a client's policy for acting on it is
    /// client-facing work; what exists today is this predicate and its leak-register row (C21).
    ///
    /// Three rules keep it from becoming something it must not be:
    ///
    /// - **It moves in one direction only.** A stale session sees *fewer* items than its principal
    ///   is entitled to — fail-closed, which is what makes an advisory answer legitimate at all.
    ///   Grant changes are not covered, and **nothing may ever be wired to make a revocation take
    ///   effect through this**: decision 0025 governs rotation, and this is not a general "the mask
    ///   changed" channel.
    /// - **It is a hint, not an expiry.** Treating a stale session as expired would need no new
    ///   wire field and is already contractual under decision 0025 — and is rejected on load: it
    ///   forces every affected session to rebuild its fragment at one tick, and the next viewport
    ///   pays a **measured 1 277 ms** row projection at 10⁹. A hint spreads the same total work over
    ///   the interval. What bounds staleness for a client that ignores it already exists:
    ///   `token_max_lifetime_secs` caps every session's life. This is the fast path, not the safety
    ///   net.
    /// - **The only remedy is a new session; [`Self::satisfied`] is never re-resolved in place.**
    ///   Re-resolving it inside a live session would break §3.4's premise 3 and with it the
    ///   patch-equals-a-rebuild equality [`Engine::fragment_for`] rests on. **This is a rule rather
    ///   than a structural impossibility** — it is the property given up to avoid the load spike
    ///   above — so it is the first thing to check in any future change to session handling.
    ///
    /// **Compaction inherits one obligation:** the dictionary length is the monotone counter this
    /// rests on, so a compaction that renumbers the dictionary must not reduce it, or must
    /// introduce a counter that never decreases.
    pub fn is_stale(&self, generation: &Generation) -> bool {
        self.unresolved_count > 0 && generation.dict.len() > self.dict_len_at_authorise
    }
}

impl Engine {
    /// Authorise a credential: `plugin.terms_of_auth` → dictionary lookup (unknown descriptors
    /// simply drop out, never an error) → `FragmentCache::get_or_build`. A zero-term credential
    /// (or one whose every descriptor is unknown) is a valid, zero-visibility session (R5) — not
    /// an error.
    ///
    /// D-G (lifecycle §3.3): `FragmentCache::get_or_build` single-flights concurrent same-key
    /// misses and doubles as an in-memory cache for warm hits (see its doc); a concurrent
    /// in-flight build on this exact canonical key surfaces here as `Err(EngineError::
    /// FragmentBuilding)` rather than blocking.
    pub fn authorise(&self, auth_data: &[u8]) -> Result<Session> {
        let auth_terms = self
            .plugin
            .terms_of_auth(auth_data)
            .map_err(EngineError::Plugin)?;

        // Loaded once, here, and used for both the dictionary and the watermark below — the
        // ordering invariant lifecycle §1.1 states: a request resolves everything against one
        // generation, or it can resolve `satisfied` against a dictionary a later flush published
        // while building a fragment against the watermark that preceded it.
        let generation = self.generation.load();

        // Counted rather than derived as `terms.len() - satisfied.len()`: `satisfied` is a set, so
        // two descriptors resolving to one ordinal would make that difference report an unresolved
        // descriptor that does not exist. Only `> 0` is ever read (`Session::is_stale`), but a
        // count that can be wrong for a reason unrelated to the dictionary is not one to keep.
        let mut satisfied: FxHashSet<TermId> = FxHashSet::default();
        // The descriptor beside each ordinal, kept for the drill-down's `labels` array — see
        // `Session::satisfied_descriptors`. Populated from the credential's own bytes and from
        // nothing else, which is what makes the surface satisfied-only by construction.
        let mut satisfied_descriptors: FxHashMap<TermId, Vec<u8>> = FxHashMap::default();
        let mut unresolved_count = 0usize;
        for descriptor in &auth_terms.terms {
            match generation.dict.lookup(descriptor) {
                Some(term) => {
                    satisfied.insert(term);
                    satisfied_descriptors.insert(term, descriptor.clone());
                }
                // An unknown descriptor is simply unsatisfied, never an error — and §3.3's
                // observation is that the ones that drop out here are precisely this session's
                // exposure to a later promotion, so the condition costs a counter to keep and a
                // rebuild to recover.
                None => unresolved_count += 1,
            }
        }

        // **`public` is added here, inside the trust boundary, and nowhere else.**
        // It is the one label every principal holds (`per-point-attributes.md` §3.8), and where it
        // is added decides what it is worth. Not as a grant, which would make the corpus's only
        // universal label depend on every credential being issued correctly; not in the plugin,
        // which is caller-supplied code deciding what a credential's bytes mean; here, after the
        // credential has been resolved and before anything is masked with the result.
        //
        // **Resolved by descriptor, not asserted as term `0`.** Every build interns it first, so
        // the two are the same number in every bundle this build writes — but a bundle whose
        // dictionary does not carry the label at all would, under a hardcoded `0`, hand every
        // principal whichever descriptor happened to be interned first. Looking the label up costs
        // one dictionary probe per authorise and cannot fail open: a bundle without it adds
        // nothing, which is the narrow direction.
        if let Some(term) = generation.dict.lookup(tessera_authz::PUBLIC_LABEL) {
            debug_assert_eq!(
                term,
                tessera_authz::PUBLIC_TERM,
                "`public` is reserved at term 0 by every build"
            );
            satisfied.insert(term);
            satisfied_descriptors.insert(term, tessera_authz::PUBLIC_LABEL.to_vec());
        }

        // **The visible-view set, resolved here and never again** (`views.md` §6) — after the
        // credential has been resolved and `public` added, and before anything is masked with the
        // result, because the gate is satisfied by exactly the terms an item's label is. Every
        // view of every group is evaluated whatever the outcome; see `crate::gate`.
        let visible_views = Arc::new(crate::gate::resolve(
            &generation.bundle.manifest,
            &generation.dict,
            &satisfied,
            self.plugin.as_ref(),
        ));

        let mut satisfied_sorted: Vec<TermId> = satisfied.iter().copied().collect();
        satisfied_sorted.sort_unstable();
        let satisfied_sorted = Arc::new(satisfied_sorted);

        // The cache's caller obligation (`FragmentCache::get_or_build`'s doc): this hash must be
        // a function of the exact `auth_data` that produced `satisfied` above, which it is.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

        let fragment = generation
            .fragments
            .get_or_build(
                &satisfied_sorted,
                auth_data_hash,
                generation.segments_version,
                &generation.postings,
                &generation.delta_postings,
                generation.watermark,
            )
            .map_err(|e| match e {
                FragmentCacheError::Building => EngineError::FragmentBuilding,
                FragmentCacheError::Io(io_err) => EngineError::Io(io_err),
            })?;

        let mut token_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let token = hex_encode(&token_bytes);

        let token_id = self.next_token_id.fetch_add(1, Ordering::Relaxed);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs();
        let expires_at = now + self.config.token_max_lifetime_secs;

        Ok(Session {
            token,
            token_id,
            satisfied,
            fragment,
            satisfied_sorted,
            satisfied_descriptors: Arc::new(satisfied_descriptors),
            visible_views,
            auth_data_hash,
            expires_at,
            unresolved_count,
            dict_len_at_authorise: generation.dict.len(),
            segments_version_at_authorise: generation.segments_version,
        })
    }

    /// Drop every cached row projection belonging to `token_id` — the revoke hook.
    ///
    /// Returns how many entries were removed, which is what
    /// `revoke_prunes_the_token` asserts on. See `RowProjectionCache::prune_token` for why this is
    /// memory hygiene rather than a disclosure control, and for the cost of the pass.
    pub fn prune_token(&self, token_id: u64) -> usize {
        // **First, and before anything is dropped.** A ladder fill still running for this token
        // would otherwise re-publish the very occupancy entries this call is removing — the
        // residue would be bounded and benign, but it would also be work done on behalf of a
        // session that no longer exists.
        self.stage.cancel(token_id);
        // **Both per-session caches**, and the second one is not optional hygiene at the campaign's
        // target: a masked-count histogram is ~4 B per artifact, 40 MB at 10⁷, and a revoked
        // session's is pinned by nothing else.
        self.masked_counts.prune_token(token_id);
        self.occupancy.retain_keys(|key| key.token_id != token_id);
        self.derived_geometry.prune_token(token_id);
        self.suggest_sets.prune_token(token_id);
        self.row_projection_cache.prune_token(token_id)
    }

    /// Drop every entry belonging to any of `token_ids` — the expiry sweep's form of
    /// [`Self::prune_token`], and the same removal with the same argument.
    ///
    /// **One pass per cache, not one prune per session.** A sweep drops a batch, and five
    /// `retain_keys` passes per session would take each of the five mutexes once per victim and
    /// re-walk every surviving key each time. Here each cache is walked once with a set membership
    /// test, so the cost is O(entries) in the batch rather than O(entries × victims). The stage
    /// cancellations stay per token: each is a map removal under its own lock, and there is no
    /// walk to share.
    ///
    /// Returns how many row projections were removed, as [`Self::prune_token`] does. Call it off
    /// the request path — `tessera_server`'s `/session/authorise` hands it to `spawn_blocking`,
    /// beside the authorisation it already runs there.
    pub fn prune_tokens(&self, token_ids: &FxHashSet<u64>) -> usize {
        if token_ids.is_empty() {
            return 0;
        }
        for token_id in token_ids {
            self.stage.cancel(*token_id);
        }
        self.masked_counts.prune_tokens(token_ids);
        self.occupancy
            .retain_keys(|key| !token_ids.contains(&key.token_id));
        self.derived_geometry.prune_tokens(token_ids);
        self.suggest_sets.prune_tokens(token_ids);
        self.row_projection_cache.prune_tokens(token_ids)
    }

    /// `session`'s mask fragment **at `generation`'s watermark** — the one thing on the request
    /// path that may stand in for `Session::fragment`.
    ///
    /// # Why a session's fragment cannot simply be the one it authorised with
    ///
    /// A fragment is materialised once per session (I2) and frozen. A flush then publishes a delta
    /// postings tier and advances the watermark, and `compose`'s rule 4 admits buffered entities at
    /// or above **the fragment's own** watermark — so an entity that a flush moved out of the
    /// buffer and into a tier falls between the two: no longer buffered, not yet in this fragment.
    /// It is invisible to that session until it re-authorises. Not fail-open — the item is missing,
    /// not wrongly shown — but it is the very property the flush exists to deliver, silently
    /// undone for exactly the sessions that were open when it happened.
    ///
    /// # Why this is a rebuild and not §11.2's patch, stated rather than glossed
    ///
    /// Design §11.2 specifies advancing the fragment by OR-ing in the flushed segment's
    /// contribution for the session's already-satisfied terms — *"a small, monotone patch rather
    /// than a rebuild"* — and write-path §4.6 sets out the four premises under which that patch is
    /// **equal** to what a rebuild produces. **This is the rebuild.** It is correct for the same
    /// reason the patch would be: `satisfied` is fixed at authorise and never re-resolved (premise
    /// 3), so the terms unioned are exactly the terms a rebuild consults.
    ///
    /// **⊘ The incremental form is not built, and is not being built** — a ruling, not a backlog
    /// entry. Decision 0044's D4 made it conditional on this being seconds-scale at 10⁹; probe P2
    /// measured **~200 ms and flat in tier count**, refuting the model. The incremental form would
    /// trade that for a ~41 ms bitmap clone, on work that had to move off the request thread
    /// anyway — and the mechanism that moved it (`crate::refresh`) is the same one the projection
    /// needed. Re-open it if a credential's satisfied set grows by orders, the base union being
    /// the whole of the 200 ms.
    ///
    /// **What that costs, and where it is now paid.** One `build_fragment_with_deltas` per
    /// *credential*, not per session: [`tessera_authz::FragmentCache`] keys on
    /// `(satisfied, auth_data_hash, resolved_at, watermark)`, so every session sharing a credential
    /// shares the build. **Measured at ~200 ms at 10⁹ and flat in tier count**
    /// (`probes/2026-08-04-refresh-ladder/` — P2, which refuted the modelled-seconds figure the
    /// corpus carried). That is three orders over decision 0044's request-path budget, so this no
    /// longer runs per tick on a request thread: the background refresh (`crate::refresh`) calls
    /// it at each publication, and a request reaches it only at establishment — rung 3 of
    /// `Engine::session_geometry`'s ladder, and `Engine::item` when this session has no resident
    /// entry at all.
    ///
    /// **Fail-closed on a busy build**: a concurrent build of the same key yields
    /// [`EngineError::FragmentBuilding`] (429) rather than a silent fall back to the stale
    /// fragment. Serving the stale one would be the quiet wrong answer this method exists to
    /// remove.
    pub(crate) fn fragment_for(
        &self,
        session: &Session,
        generation: &Generation,
    ) -> Result<Arc<FrozenFragment>> {
        // **Both tests, and the identity one is not redundant.** A flush advances the watermark, so
        // the watermark alone decides whether a session's own fragment is still current *within* a
        // prefix. A fold advances no watermark at all — it rewrites the term index and rotates the
        // bundle identity (decision 0050) — so on the watermark test alone a session authorised
        // before a fold would go on composing against a fragment that still contains every entity
        // the fold retired. That is Rule F re-exposing exactly what it withdrew, and it is why
        // compaction §4 puts the comparison *here*, at composition: this fragment is held by
        // `Session`, outside `FragmentCache` altogether, so rotating the cache does not reach it.
        if session.fragment.identity == generation.bundle_identity()
            && session.fragment.watermark >= generation.watermark
        {
            return Ok(Arc::clone(&session.fragment));
        }
        generation
            .fragments
            .get_or_build(
                &session.satisfied_sorted,
                session.auth_data_hash,
                // **The generation the session's `satisfied` was resolved against, not the live
                // one** (#112). `get_or_build` memoises `auth_data_hash → canonical key` under
                // `(hash, resolved_at, watermark)`, and its caller obligation is that the hash and
                // the stamp "must never arrive paired with two different term sets". This session's
                // `satisfied` was frozen at authorise, against the generation as it stood then;
                // passing the *live* stamp pairs a stale term set with a generation that has moved
                // past it, and the memo keeps that pairing.
                //
                // The cost was not to this session, which is stale either way until it
                // re-authorises (`Session::is_stale`). It was to the **next** authorise of the same
                // credential: that one resolves the promoted descriptor correctly, hits the entry
                // this call left behind, and is handed the fragment for the grant set it has just
                // stopped having — a session served its pre-flush visible set indefinitely, with
                // nothing later repairing it. Same bytes took the poisoned path and different bytes
                // naming the same terms did not, which is the asymmetry that identified it.
                session.segments_version_at_authorise,
                &generation.postings,
                &generation.delta_postings,
                generation.watermark,
            )
            .map_err(|e| match e {
                FragmentCacheError::Building => EngineError::FragmentBuilding,
                FragmentCacheError::Io(io_err) => EngineError::Io(io_err),
            })
    }

    /// Which layers this principal may know exist, and which of those are currently served.
    ///
    /// **Two questions, answered in that order, and the order is the disclosure control.**
    /// Reachability is resolved from the registry — one set probe, identical for a gate-failed name
    /// and a never-registered one. What that resolution must *not* carry is the verdict: a
    /// suppression against a layer's own entity takes effect at the ack, so the overlay is asked
    /// live, per call, for every name the resolution admitted. A cache may bake in reachability;
    /// it may never bake in whether a layer is currently served.
    ///
    /// A suppressed layer therefore leaves the resolved set the way a gate-failed one never
    /// entered it — same answer, and by the same route the request path already takes for a point.
    pub fn visible_layers(&self, session: &Session) -> Vec<tessera_types::layer::RegisteredLayer> {
        let generation = self.generation();
        let resolved = self.write.live().resolve_layers(
            |term| session.satisfied.contains(&term),
            |label| generation.dict.lookup(label.as_bytes()),
        );
        resolved
            .names()
            .filter_map(|name| self.write.live().registered_layer(name))
            .filter(|layer| {
                // **The live half, and it is asked per call.** A layer's own entity carries its
                // suppression, so this is the same deleted-beats-suppressed composition a point
                // goes through, against the current overlay rather than against whatever was true
                // when the reachable set was resolved. `None` — no opinion — is *not* visible here:
                // a layer has no row and no fragment to fall through to, so the only honest reading
                // of "nothing says yes" is no.
                !generation.overlay.is_deleted(layer.entity)
                    && !generation.overlay.is_suppressed(layer.entity)
            })
            .collect()
    }
}
