//! Session authorisation.
//!
//! [`Engine::authorise`] turns a credential into a [`Session`]: granted descriptors are resolved
//! against the bundle dictionary and the resulting term set is unioned into a mask fragment via
//! [`FragmentCache`]. That union is the authorisation decision.

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
/// unions to, computed once here and reused rather than recomputed per viewport.
///
/// Holds no entity-id → wire-handle table: that state lives in `tessera-server`, alongside this
/// struct rather than inside it, so this crate never depends on `tessera-wire`'s handle type.
pub struct Session {
    /// Bearer token: 32 random bytes, hex-encoded.
    token: String,
    /// A process-local identity for this session, distinct from `token`: part of the
    /// row-projection cache key, so the cache never hashes or compares the full token string.
    token_id: u64,
    /// The credential's granted terms, resolved to bundle-relative `TermId`s. An unknown descriptor
    /// is absent here, never an error. Resolved once, at authorise, and never re-resolved in
    /// place: see [`Session::is_stale`]. A term promoted after authorise is not added; the remedy
    /// is a new session.
    satisfied: FxHashSet<TermId>,
    /// The materialised mask fragment: the union of every satisfied term's postings, over the base
    /// and every delta tier live when this session authorised. Goes stale as flushes publish delta
    /// tiers and move the watermark: a flushed entity is invisible to a viewer served this
    /// fragment until it is rebuilt. [`Engine::fragment_for`] is what brings it forward, and is the
    /// only reader of this field outside a test hook.
    fragment: Arc<FrozenFragment>,
    /// `satisfied`, sorted: the [`tessera_authz::FragmentCache`] key component, kept rather than
    /// re-sorted per request. `Arc` so the row-projection cache can carry it for the background
    /// refresh, which has no session registry to look it up in.
    satisfied_sorted: Arc<Vec<TermId>>,
    /// The descriptor the credential presented for each satisfied term: the only route by which a
    /// term ordinal becomes a string a viewer is shown. The drill-down's `labels` array is built
    /// from this map alone, intersected with an entity's own term list, so a term absent here has
    /// no name and cannot be served: a bug here can only lose a label the viewer holds, never
    /// invent one they do not. Holds `public` too, with no credential behind it, for the same
    /// reason it is in [`Session::satisfied`].
    satisfied_descriptors: Arc<FxHashMap<TermId, Vec<u8>>>,
    /// Every view of every group this principal may reach, resolved once at authorise and fixed
    /// for the session's life. Every view is evaluated whatever the outcome, so a gate-failed name
    /// costs the same lookup as a name nobody declared, and a view created after authorise is a
    /// 404 to this session until it re-authorises. `Arc` because every request path reads it and
    /// none may clone the set.
    visible_views: Arc<crate::gate::VisibleViews>,
    /// `sha256(auth_data)`, which the fragment cache asks for. The credential itself is not kept.
    auth_data_hash: [u8; 32],
    /// Unix timestamp (seconds) after which this session is no longer valid.
    expires_at: u64,
    /// How many of the credential's granted descriptors had no dictionary entry at authorise. No
    /// accessor: this count says how many of the viewer's descriptors the corpus does not carry,
    /// which is more than [`Session::is_stale`]'s boolean and must not reach the wire.
    unresolved_count: usize,
    /// See [`Self::unresolved_count`].
    dict_len_at_authorise: u32,
}

impl Session {
    /// The bearer token this session is presented with.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// This session's process-local identity, the cache-key component.
    pub fn token_id(&self) -> u64 {
        self.token_id
    }

    /// The Unix timestamp (seconds) after which this session is no longer valid.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Every view of every group this principal may reach.
    pub fn visible_views(&self) -> &crate::gate::VisibleViews {
        &self.visible_views
    }

    /// The credential's granted terms. Term ids are internal and never reach a client.
    pub(crate) fn satisfied(&self) -> &FxHashSet<TermId> {
        &self.satisfied
    }

    /// [`Self::satisfied`] sorted, the fragment-cache key component.
    pub(crate) fn satisfied_sorted(&self) -> &Arc<Vec<TermId>> {
        &self.satisfied_sorted
    }

    /// The descriptor the credential presented for each satisfied term.
    pub(crate) fn satisfied_descriptors(&self) -> &Arc<FxHashMap<TermId, Vec<u8>>> {
        &self.satisfied_descriptors
    }

    /// `sha256(auth_data)`.
    pub(crate) fn auth_data_hash(&self) -> [u8; 32] {
        self.auth_data_hash
    }


    /// Whether this session's mask may be behind the corpus: true iff the credential named a
    /// descriptor the dictionary did not carry at authorise, and the dictionary has grown since.
    /// A session with nothing unresolved is never hinted, and a stale session sees fewer items
    /// than its principal is entitled to, never more. It is a hint, not a revocation; the only
    /// remedy is a new session, since `satisfied` is never re-resolved in place.
    ///
    /// A compaction that renumbers the dictionary must not reduce its length, or must keep a
    /// counter that never decreases: this predicate rests on that length being monotone.
    pub fn is_stale(&self, generation: &Generation) -> bool {
        self.unresolved_count > 0 && generation.dict.len() > self.dict_len_at_authorise
    }
}

impl Engine {
    /// Authorise a credential: `plugin.terms_of_auth` → dictionary lookup (an unknown descriptor
    /// drops out, never an error) → `FragmentCache::get_or_build`. A zero-term credential, or one
    /// whose every descriptor is unknown, is a valid, zero-visibility session, not an error. A
    /// concurrent in-flight build on the same key surfaces here as
    /// `Err(EngineError::FragmentBuilding)` rather than blocking.
    pub fn authorise(&self, auth_data: &[u8]) -> Result<Session> {
        let auth_terms = self
            .plugin
            .terms_of_auth(auth_data)
            .map_err(EngineError::Plugin)?;

        // Loaded once and used for both the dictionary and the watermark below: resolving them
        // against different generations could pair `satisfied` with a dictionary a later flush
        // published while building a fragment against the watermark that preceded it.
        let generation = self.generation.load();

        let mut satisfied: FxHashSet<TermId> = FxHashSet::default();
        let mut satisfied_descriptors: FxHashMap<TermId, Vec<u8>> = FxHashMap::default();
        let mut unresolved_count = 0usize;
        for descriptor in &auth_terms {
            match generation.dict.lookup(descriptor) {
                Some(term) => {
                    satisfied.insert(term);
                    satisfied_descriptors.insert(term, descriptor.clone());
                }
                // An unknown descriptor is unsatisfied, never an error: it is this session's
                // exposure to a later promotion of that same descriptor.
                None => unresolved_count += 1,
            }
        }

        // Every session holds `public`, and this is the only place it is added: not by the plugin
        // and not by a grant. Looked up by descriptor, because in a bundle whose dictionary lacks
        // it term 0 is some other label, and a hardcoded 0 would grant that to everyone.
        if let Some(term) = generation.dict.lookup(tessera_authz::PUBLIC_LABEL) {
            debug_assert_eq!(
                term,
                tessera_authz::PUBLIC_TERM,
                "`public` is reserved at term 0 by every build"
            );
            satisfied.insert(term);
            satisfied_descriptors.insert(term, tessera_authz::PUBLIC_LABEL.to_vec());
        }

        // Resolved after the credential and `public` are both in `satisfied`, since a gate is
        // satisfied by exactly the terms an item's label is.
        let visible_views = Arc::new(crate::gate::resolve(
            &generation.bundle.manifest,
            &generation.dict,
            &satisfied,
            self.plugin.as_ref(),
        ));

        let mut satisfied_sorted: Vec<TermId> = satisfied.iter().copied().collect();
        satisfied_sorted.sort_unstable();
        let satisfied_sorted = Arc::new(satisfied_sorted);

        // Must be a function of the exact `auth_data` that produced `satisfied` above.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

        let fragment = generation
            .fragments
            .get_or_build(
                &satisfied_sorted,
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
        })
    }

    /// Drop every cached entry belonging to `token_id` — the revoke hook, memory hygiene rather
    /// than a disclosure control. Returns how many row projections were removed.
    pub fn prune_token(&self, token_id: u64) -> usize {
        // Cancelled first: a ladder fill still running for this token would otherwise re-publish
        // the occupancy entries this call is removing.
        self.stage.cancel(token_id);
        self.masked_counts.prune_token(token_id);
        self.occupancy.retain_keys(|key| key.token_id != token_id);
        self.derived_geometry.prune_token(token_id);
        self.suggest_sets.prune_token(token_id);
        self.row_projection_cache.prune_token(token_id)
    }

    /// The expiry sweep's form of [`Self::prune_token`]: drops every entry belonging to any of
    /// `token_ids`, each cache walked once with a set-membership test rather than once per victim.
    /// Call it off the request path. Returns how many row projections were removed.
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

    /// `session`'s mask fragment at `generation`'s watermark — what the request path uses in place
    /// of the frozen `Session::fragment`.
    ///
    /// A fragment is frozen at authorise. A flush advances the watermark, which the watermark test
    /// alone catches. A fold advances no watermark; it rewrites the term index and rotates the
    /// bundle identity instead, so the identity comparison is what makes a session authorised
    /// before a fold rebuild its fragment against the new term index. That comparison is not the
    /// only protection for a retired entity against such a session: the folded row space and the
    /// folded value columns no longer hold the entity either.
    ///
    /// Rebuilds the union at the current watermark rather than patching the frozen fragment
    /// forward: `satisfied` is fixed at authorise and never re-resolved, so the terms unioned here
    /// are exactly the terms a rebuild would consult. Measured at ~200 ms at 10⁹, which is why this
    /// runs from the background refresh (`crate::refresh`) at each publication rather than per
    /// request; a request reaches it directly only at session establishment.
    ///
    /// A concurrent build of the same key yields [`EngineError::FragmentBuilding`] rather than
    /// falling back to the stale fragment.
    pub(crate) fn fragment_for(
        &self,
        session: &Session,
        generation: &Generation,
    ) -> Result<Arc<FrozenFragment>> {
        // The identity test catches what the watermark test cannot: a fold rotates the bundle
        // identity without moving the watermark, and this fragment lives outside `FragmentCache`,
        // so rotating the cache does not reach it.
        if session.fragment.identity == generation.bundle_identity()
            && session.fragment.watermark >= generation.watermark
        {
            return Ok(Arc::clone(&session.fragment));
        }
        generation
            .fragments
            .get_or_build(
                &session.satisfied_sorted,
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
    /// Two questions, answered in that order. Reachability is resolved from the registry: one set
    /// probe, identical for a gate-failed name and a never-registered one. Whether a reachable
    /// layer is currently served is asked live, per call, against the overlay, since a suppression
    /// takes effect at the ack: a cache may bake in reachability but must never bake in whether a
    /// layer is served.
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
                // Checked live, per call, against the current overlay: a layer's own entity
                // carries its suppression. A layer has no fragment to fall through to, so no
                // opinion here means not visible.
                !generation.overlay.is_deleted(layer.entity)
                    && !generation.overlay.is_suppressed(layer.entity)
            })
            .collect()
    }
}

/// The two fields a test may read directly. They live here rather than in `crate::test_hooks`
/// because the fields are private to this module.
impl Session {
    /// The fragment frozen at authorise, stale by construction after any flush or fold.
    #[cfg(any(feature = "fault-injection", feature = "bench-timing"))]
    #[doc(hidden)]
    pub fn fragment_at_authorise_for_test(&self) -> &Arc<FrozenFragment> {
        &self.fragment
    }

    /// The term set resolved at authorise.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn satisfied_for_test(&self) -> &FxHashSet<TermId> {
        &self.satisfied
    }
}
