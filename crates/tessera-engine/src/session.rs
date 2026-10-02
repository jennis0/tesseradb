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
    /// The index keys the credential satisfies, as bundle-relative `TermId`s: each term it holds
    /// that the dictionary carries, `public`, and the key of each label holding a conjunction
    /// that its terms satisfy. A term the dictionary does not carry is absent here, never an
    /// error. Resolved once, at authorise, and never re-resolved in place: see
    /// [`Session::is_stale`]. A key promoted after authorise is not added; the remedy is a new
    /// session.
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
    /// The term the credential presented for each satisfied term ordinal, and `public`. A label
    /// key has no entry: the item card names it by a witness drawn from [`Self::credentials`].
    satisfied_descriptors: Arc<FxHashMap<TermId, Vec<u8>>>,
    /// Every term the credential holds, whether or not the dictionary carries it, and `public`.
    /// What a view's, a layer's or an artifact's own label is evaluated against, so a label no
    /// item carries is still one a credential can satisfy. The item card's witness names only
    /// terms from this set.
    credentials: Arc<FxHashSet<Vec<u8>>>,
    /// Every view of every group this principal may reach, resolved once at authorise and fixed
    /// for the session's life. Every view is evaluated whatever the outcome, so a gate-failed name
    /// costs the same lookup as a name nobody declared, and a view created after authorise is a
    /// 404 to this session until it re-authorises. `Arc` because every request path reads it and
    /// none may clone the set.
    visible_views: Arc<crate::gate::VisibleViews>,
    /// `sha256(auth_data)`, part of the identity a client's held frames are keyed on. The
    /// credential itself is not kept.
    auth_data_hash: [u8; 32],
    /// Unix timestamp (seconds) after which this session is no longer valid.
    expires_at: u64,
    /// The dictionary's length at authorise: every key from here on was promoted since.
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

    /// Whether the credential holds the term `term`.
    pub(crate) fn holds(&self, term: &str) -> bool {
        self.credentials.contains(term.as_bytes())
    }

    /// Whether the credential satisfies any of the stored `labels`.
    pub(crate) fn admits<S: AsRef<str>>(&self, labels: &[S]) -> bool {
        tessera_types::label::admits(labels, &|term| self.holds(term))
    }

    /// `sha256(auth_data)`.
    pub(crate) fn auth_data_hash(&self) -> [u8; 32] {
        self.auth_data_hash
    }


    /// Whether this session's mask is behind the corpus: true iff a key promoted since authorise
    /// is a term the credential holds, or a label holding a conjunction that its terms satisfy. A
    /// stale session sees fewer items than its principal is entitled to, never more. It is a
    /// hint, not a revocation; the only remedy is a new session, since `satisfied` is never
    /// re-resolved in place.
    ///
    /// A compaction that renumbers the dictionary must not reduce its length, or must keep a
    /// counter that never decreases: this predicate rests on that length being monotone.
    pub fn is_stale(&self, generation: &Generation) -> bool {
        let labels = generation.dict.labels();
        (self.dict_len_at_authorise..generation.dict.len())
            .map(TermId::new)
            .any(|key| match labels.is_label_key(key) {
                true => labels.satisfied(key, &|term| self.holds(term)),
                false => generation
                    .dict
                    .descriptor(key)
                    .is_some_and(|term| self.credentials.contains(term)),
            })
    }
}

impl Engine {
    /// Authorise a credential: its terms → dictionary lookup (an unknown term drops out, never an
    /// error), and the labels holding a conjunction that those terms satisfy, from the DAG → the
    /// union of every satisfied key's postings, through `FragmentCache::get_or_build`. A zero-term
    /// credential, or one whose every term is unknown, is a valid, zero-visibility session, not an
    /// error. A concurrent in-flight build on the same key surfaces here as
    /// `Err(EngineError::FragmentBuilding)` rather than blocking.
    pub fn authorise(&self, auth_data: &[u8]) -> Result<Session> {
        let auth_terms = credential_terms(auth_data)?;

        // Loaded once and used for both the dictionary and the watermark below: resolving them
        // against different generations could pair `satisfied` with a dictionary a later flush
        // published while building a fragment against the watermark that preceded it.
        let generation = self.generation.load();

        let mut credentials: FxHashSet<Vec<u8>> =
            auth_terms.iter().map(|t| t.as_bytes().to_vec()).collect();
        credentials.insert(tessera_authz::PUBLIC_LABEL.to_vec());
        let mut satisfied: FxHashSet<TermId> = FxHashSet::default();
        let mut satisfied_descriptors: FxHashMap<TermId, Vec<u8>> = FxHashMap::default();
        let labels = generation.dict.labels();
        for term in &auth_terms {
            // A held term never names a label's own key, which starts with a control character
            // `credential_terms` drops; the test keeps a key out of `satisfied` whatever a
            // credential presents. An unknown term is unsatisfied, never an error.
            if let Some(id) = generation.dict.lookup(term.as_bytes()) {
                if !labels.is_label_key(id) {
                    satisfied.insert(id);
                    satisfied_descriptors.insert(id, term.as_bytes().to_vec());
                }
            }
        }
        let mut keys = Vec::new();
        labels.authorise(auth_terms.iter().map(String::as_str), &mut keys);
        satisfied.extend(keys);

        // Every session holds `public`, and this is the only place it is added: not by a
        // credential and not by a grant. Looked up by descriptor, because in a bundle whose
        // dictionary lacks it term 0 is some other label, and a hardcoded 0 would grant that to
        // everyone.
        if let Some(term) = generation.dict.lookup(tessera_authz::PUBLIC_LABEL) {
            debug_assert_eq!(
                term,
                tessera_authz::PUBLIC_TERM,
                "`public` is reserved at term 0 by every build"
            );
            satisfied.insert(term);
            satisfied_descriptors.insert(term, tessera_authz::PUBLIC_LABEL.to_vec());
        }

        let visible_views = Arc::new(crate::gate::resolve(
            &generation.bundle.manifest,
            &credentials,
        ));

        let mut satisfied_sorted: Vec<TermId> = satisfied.iter().copied().collect();
        satisfied_sorted.sort_unstable();
        let satisfied_sorted = Arc::new(satisfied_sorted);

        // Must be a function of the exact `auth_data` that produced `satisfied` above.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

        let fragment = generation
            .fragments
            .get_or_build_waiting(
                &satisfied_sorted,
                &generation.postings,
                &generation.delta_postings,
                generation.watermark,
                &tessera_cache::NeverCancelled,
            )
            .map_err(|e| match e {
                FragmentCacheError::Building => EngineError::FragmentBuilding,
                FragmentCacheError::Cancelled => EngineError::Cancelled,
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
            credentials: Arc::new(credentials),
            visible_views,
            auth_data_hash,
            expires_at,
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
            .get_or_build_waiting(
                &session.satisfied_sorted,
                &generation.postings,
                &generation.delta_postings,
                generation.watermark,
                &tessera_cache::NeverCancelled,
            )
            .map_err(|e| match e {
                FragmentCacheError::Building => EngineError::FragmentBuilding,
                FragmentCacheError::Cancelled => EngineError::Cancelled,
                FragmentCacheError::Io(io_err) => EngineError::Io(io_err),
            })
    }

    /// Which layers this principal may know exist. A layer's label is evaluated against the
    /// credential's terms, the test an artifact's own label takes. A gate-failed name and a
    /// never-registered one answer identically, so a name outside this set reveals nothing about
    /// why.
    pub(crate) fn reachable_layers(&self, session: &Session) -> tessera_lifecycle::ResolvedLayers {
        self.write
            .live()
            .resolve_layers(|label| session.admits(&[label]))
    }

    /// The label test for one layer's artifacts, for this session: an artifact's own label is
    /// admitted when the credential satisfies any of its labels, and an artifact with none by the
    /// layer's `artifact_visibility.default`.
    pub(crate) fn label_gate<'s>(
        &self,
        session: &'s Session,
        declaration: &tessera_types::layer::LayerDeclaration,
    ) -> crate::artifacts::LabelGate<'s> {
        use tessera_types::layer::MemberDefault;
        // A layer naming no field has no artifact labels, and its default says nothing.
        let unlabelled = !declaration.artifact_visibility.carries_own_labels()
            || match &declaration.artifact_visibility.default {
                MemberDefault::Inherited => true,
                MemberDefault::Label(label) => session.admits(&[label]),
            };
        crate::artifacts::LabelGate::new(&session.credentials, unlabelled)
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
        self.reachable_layers(session)
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

/// The terms a credential presents. `auth_data` is the JSON `{"terms": ["<term>", ...]}`; each
/// term is held as [`tessera_types::label::held_term`] says, and one it refuses is dropped. A
/// credential that parses to no terms is valid, and its session sees what `public` admits.
fn credential_terms(auth_data: &[u8]) -> Result<Vec<String>> {
    let refused = || {
        EngineError::Credential(
            "`auth_data` must be JSON of the form {\"terms\": [\"<term>\", ...]}".to_string(),
        )
    };
    let value: serde_json::Value = serde_json::from_slice(auth_data).map_err(|_| refused())?;
    let terms = value
        .get("terms")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(refused)?;
    let mut held = Vec::with_capacity(terms.len());
    for term in terms {
        let term = term.as_str().ok_or_else(refused)?;
        held.extend(tessera_types::label::held_term(term).map(str::to_owned));
    }
    held.sort_unstable();
    held.dedup();
    Ok(held)
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
