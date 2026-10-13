//! Session authorisation.
//!
//! [`Engine::authorise`] turns a credential into a [`Session`]: granted descriptors are resolved
//! against the bundle dictionary, and the resulting term set is what each shard's mask fragment
//! unions in the [`FragmentCache`]. That union is the authorisation decision.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use mosaica_authz::{FragmentCacheError, FrozenFragment, Grant};
use mosaica_types::TermId;

#[cfg(doc)]
use mosaica_authz::FragmentCache;

use crate::engine::{hex_encode, Engine};
use crate::error::{EngineError, Result};
use crate::Generation;

/// The index keys a session satisfies: those it resolved at authorise, or every key the
/// dictionary carries now or later.
#[derive(Clone)]
pub enum Satisfied {
    /// These keys, and no other.
    Keys(FxHashSet<TermId>),
    /// Every key, whenever it was promoted: a `read-all` session.
    Every,
}

/// A set of index keys, asked one key at a time. [`Satisfied`] is a session's; a plain set of
/// keys answers as [`Satisfied::Keys`] does.
pub trait SatisfiedKeys {
    /// Whether `key` is in the set.
    fn holds_key(&self, key: TermId) -> bool;
}

impl SatisfiedKeys for Satisfied {
    fn holds_key(&self, key: TermId) -> bool {
        match self {
            Satisfied::Keys(keys) => keys.contains(&key),
            Satisfied::Every => true,
        }
    }
}

impl SatisfiedKeys for FxHashSet<TermId> {
    fn holds_key(&self, key: TermId) -> bool {
        self.contains(&key)
    }
}

/// What a session authorised for a set of terms holds. A `read-all` session has none of this: it
/// holds every term.
struct HeldTerms {
    /// The term the credential presented for each satisfied term ordinal, and `public`. A label
    /// key has no entry: the item card names it by a witness drawn from [`Self::credentials`].
    descriptors: FxHashMap<TermId, Vec<u8>>,
    /// Every term the credential holds, whether or not the dictionary carries it, and `public`.
    /// What a view's, a layer's or an artifact's own label is evaluated against, so a label no
    /// item carries is still one a credential can satisfy. The item card's witness names only
    /// terms from this set.
    credentials: FxHashSet<Vec<u8>>,
}

/// One authorised viewer session: the index keys it satisfies, resolved once here. The mask
/// fragment they union to is built per shard on first use and brought forward by the background
/// refresh rather than recomputed per viewport.
///
/// Holds no entity-id → wire-handle table: that state lives in `mosaica-server`, alongside this
/// struct rather than inside it, so this crate never depends on `mosaica-wire`'s handle type.
pub struct Session {
    /// Bearer token: 32 random bytes, hex-encoded.
    token: String,
    /// A process-local identity for this session, distinct from `token`: part of every
    /// per-session cache key, so no cache hashes or compares the full token string.
    token_id: u64,
    /// The index keys the session satisfies. For a session authorised for terms, as
    /// bundle-relative `TermId`s: each term it holds that the dictionary carries, `public`, and
    /// the key of each conjunction that its terms satisfy. A term the dictionary
    /// does not carry is absent, never an error. Resolved once, at authorise, and never
    /// re-resolved in place: see [`Session::is_stale`]. For a `read-all` session, every key.
    satisfied: Satisfied,
    /// What the fragment is the union of, and the [`mosaica_authz::FragmentCache`] key component:
    /// `satisfied`'s keys sorted, or every key. A `read-all` session's fragment is keyed by the
    /// watermark alone, so every such session at one watermark shares it.
    grant: Grant,
    /// The terms the session holds, or `None` for a `read-all` session.
    held: Option<HeldTerms>,
    /// `sha256` over `grant`: what a cache shared by every session with this grant is keyed on
    /// ([`crate::figures::MaskIdentity::terms`], the row projection's key).
    terms_digest: [u8; 32],
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
    /// The newest generation this session has been served each view at, per shard: only a view it
    /// has been served may be answered from its grant's previous generation.
    served: std::sync::Mutex<Vec<Served>>,
}

/// A view of a shard a session has been served, and the newest generation it was served at.
struct Served {
    view: String,
    shard: mosaica_types::ShardId,
    segments_version: u64,
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

    /// The index keys the session satisfies. Term ids are internal and never reach a client.
    pub(crate) fn satisfied(&self) -> &Satisfied {
        &self.satisfied
    }

    /// What the session's fragment is the union of.
    pub(crate) fn grant(&self) -> &Grant {
        &self.grant
    }

    /// See [`Self::terms_digest`].
    pub(crate) fn terms_digest(&self) -> [u8; 32] {
        self.terms_digest
    }

    /// The term the session's item card names for the index key `key`, where `key` is a term the
    /// session holds. A conjunction's key names none: the card writes a clause of it. A
    /// `read-all` session holds every term, so every term key is named from `dict`.
    pub(crate) fn term_of<'a>(
        &'a self,
        key: TermId,
        dict: &'a mosaica_authz::Dict,
    ) -> Option<&'a [u8]> {
        match &self.held {
            Some(held) => held.descriptors.get(&key).map(Vec::as_slice),
            None if dict.labels().is_conjunction_key(key) => None,
            None => dict.descriptor(key),
        }
    }

    /// Whether the session holds the term `term`. A `read-all` session holds every term.
    pub(crate) fn holds(&self, term: &str) -> bool {
        self.held
            .as_ref()
            .is_none_or(|held| held.credentials.contains(term.as_bytes()))
    }

    /// Whether the session satisfies any of the stored `labels`.
    pub(crate) fn admits<S: AsRef<str>>(&self, labels: &[S]) -> bool {
        mosaica_access::admits(labels, &|term| self.holds(term))
    }

    fn served(&self) -> std::sync::MutexGuard<'_, Vec<Served>> {
        self.served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The newest generation this session has been served `view` of `shard` at.
    pub(crate) fn served_at(&self, view: &str, shard: mosaica_types::ShardId) -> Option<u64> {
        self.served()
            .iter()
            .find(|served| served.view == view && served.shard == shard)
            .map(|served| served.segments_version)
    }

    /// Whether this session has been served any view of `shard`.
    pub(crate) fn served_in(&self, shard: mosaica_types::ShardId) -> bool {
        self.served().iter().any(|served| served.shard == shard)
    }

    /// Record that this session was served `view` of `shard` at `segments_version`.
    pub(crate) fn record_served(
        &self,
        view: &str,
        shard: mosaica_types::ShardId,
        segments_version: u64,
    ) {
        let mut served = self.served();
        match served
            .iter_mut()
            .find(|served| served.view == view && served.shard == shard)
        {
            Some(served) => {
                served.segments_version = served.segments_version.max(segments_version)
            }
            None => served.push(Served {
                view: view.to_string(),
                shard,
                segments_version,
            }),
        }
    }

    /// `sha256(auth_data)`.
    pub(crate) fn auth_data_hash(&self) -> [u8; 32] {
        self.auth_data_hash
    }

    /// Whether this session's mask is behind the corpus: true iff a key promoted since authorise
    /// is a term the session holds, or a conjunction that its terms satisfy. A
    /// stale session sees fewer items than its principal is entitled to, never more. It is a
    /// hint, not a revocation; the only remedy is a new session, since `satisfied` is never
    /// re-resolved in place. A `read-all` session is never behind: its fragment is built from
    /// every key at each refresh.
    ///
    /// A compaction that renumbers the dictionary must not reduce its length, or must keep a
    /// counter that never decreases: this predicate rests on that length being monotone.
    pub fn is_stale(&self, generation: &Generation) -> bool {
        let Some(held) = &self.held else {
            return false;
        };
        let labels = generation.dict.labels();
        (self.dict_len_at_authorise..generation.dict.len())
            .map(TermId::new)
            .any(|key| match labels.is_conjunction_key(key) {
                true => labels.satisfied(key, &|term| self.holds(term)),
                false => generation
                    .dict
                    .descriptor(key)
                    .is_some_and(|term| held.credentials.contains(term)),
            })
    }
}

/// The authorisation data [`Engine::authorise_all`] hashes into its sessions' cursors. A
/// credential is JSON, which cannot begin with a NUL byte.
const AUTH_DATA_EVERY_ITEM: &[u8] = b"\0every item";

impl Engine {
    /// Authorise a credential: its terms → dictionary lookup (an unknown term drops out, never an
    /// error), and the conjunctions that those terms satisfy, from the DAG. The union of every
    /// satisfied key's postings is built per shard when the session first reads it
    /// ([`Engine::fragment_for`]). A zero-term credential, or one whose every term is unknown, is a
    /// valid, zero-visibility session, not an error.
    pub fn authorise(&self, auth_data: &[u8]) -> Result<Session> {
        let auth_terms = credential_terms(auth_data)?;

        // Loaded once, so `satisfied`, the views resolved and the dictionary length recorded all
        // describe one generation: a dictionary a later flush published could otherwise be
        // recorded as seen while `satisfied` was resolved against the one before it.
        let generation = self.generation.load();

        let mut credentials: FxHashSet<Vec<u8>> =
            auth_terms.iter().map(|t| t.as_bytes().to_vec()).collect();
        credentials.insert(mosaica_authz::PUBLIC_LABEL.to_vec());
        let mut satisfied: FxHashSet<TermId> = FxHashSet::default();
        let mut descriptors: FxHashMap<TermId, Vec<u8>> = FxHashMap::default();
        let labels = generation.dict.labels();
        for term in &auth_terms {
            // A held term never names a conjunction's key, which starts with a control character
            // `credential_terms` drops; the test keeps a key out of `satisfied` whatever a
            // credential presents. An unknown term is unsatisfied, never an error.
            if let Some(id) = generation.dict.lookup(term.as_bytes()) {
                if !labels.is_conjunction_key(id) {
                    satisfied.insert(id);
                    descriptors.insert(id, term.as_bytes().to_vec());
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
        if let Some(term) = generation.dict.lookup(mosaica_authz::PUBLIC_LABEL) {
            debug_assert_eq!(
                term,
                mosaica_authz::PUBLIC_TERM,
                "`public` is reserved at term 0 by every build"
            );
            satisfied.insert(term);
            descriptors.insert(term, mosaica_authz::PUBLIC_LABEL.to_vec());
        }

        let mut sorted: Vec<TermId> = satisfied.iter().copied().collect();
        sorted.sort_unstable();
        let held = HeldTerms {
            descriptors,
            credentials,
        };
        self.mint(
            &generation,
            auth_data,
            Satisfied::Keys(satisfied),
            Grant::Keys(Arc::new(sorted)),
            Some(held),
        )
    }

    /// Authorise a session that satisfies every index key, whenever it was promoted, and so every
    /// view's, layer's and artifact's label. Its fragment is the union of every posting at the
    /// watermark it is built for, under one cache entry that every such session shares, and the
    /// background refresh rebuilds it at each publication, so an item a flush places under a new
    /// term or a new label joins it. Deletions and suppressions are removed from it when the
    /// visible set is composed, as from any session's.
    pub fn authorise_all(&self) -> Result<Session> {
        let generation = self.generation.load();
        self.mint(
            &generation,
            AUTH_DATA_EVERY_ITEM,
            Satisfied::Every,
            Grant::Every,
            None,
        )
    }

    /// A session for `satisfied`, whose fragment in each shard is `grant`'s union.
    fn mint(
        &self,
        generation: &Generation,
        auth_data: &[u8],
        satisfied: Satisfied,
        grant: Grant,
        held: Option<HeldTerms>,
    ) -> Result<Session> {
        let visible_views = Arc::new(crate::gate::resolve(
            &generation.bundle.manifest,
            &|term| {
                held.as_ref()
                    .is_none_or(|held| held.credentials.contains(term.as_bytes()))
            },
        ));

        // Four bytes a key; `Every` hashes a five-byte input, so no key set shares its digest.
        let terms_digest: [u8; 32] = match &grant {
            Grant::Keys(keys) => {
                let mut hasher = Sha256::new();
                for key in keys.iter() {
                    hasher.update(key.raw().to_le_bytes());
                }
                hasher.finalize().into()
            }
            Grant::Every => Sha256::digest(b"every").into(),
        };

        // Must be a function of the exact `auth_data` that produced `satisfied`.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

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
            grant,
            held,
            terms_digest,
            visible_views,
            auth_data_hash,
            expires_at,
            dict_len_at_authorise: generation.dict.len(),
            served: Default::default(),
        })
    }

    /// Drop every cached entry belonging to any of `token_ids`, each cache walked once with a
    /// set-membership test rather than once per victim: memory hygiene when sessions end, not a
    /// disclosure control. Row projections are their grants', and stay until the byte bound or a
    /// publication's retention removes them. Call it off the request path.
    pub fn prune_tokens(&self, token_ids: &FxHashSet<u64>) {
        if token_ids.is_empty() {
            return;
        }
        self.slots.end(token_ids);
        for token_id in token_ids {
            self.stage.cancel(*token_id);
        }
        self.occupancy
            .retain_keys(|key| !token_ids.contains(&key.token_id));
        self.derived_geometry.prune_tokens(token_ids);
        self.suggest_sets.prune_tokens(token_ids);
        self.figures.prune_tokens(token_ids);
    }

    /// `session`'s mask fragment in `shard`, at its watermark and under its identity, from the
    /// fragment cache, built there on a miss. A flush moves the watermark and a fold the identity,
    /// so neither is answered from a fragment of the term index or tiers before it. That is not
    /// the only protection for a retired entity against a session authorised before a fold: the
    /// folded row space and the folded value columns no longer hold the entity either.
    ///
    /// `satisfied` is fixed at authorise and never re-resolved, so the terms unioned here are
    /// exactly the terms any build for this session consults. A build is measured at ~200 ms at
    /// 10⁹, which is why the background refresh (`crate::refresh`) makes the next one at each
    /// publication; a request reaches a build directly only on a session's first use of a shard.
    ///
    /// `held` is a fragment of this session's the request already holds, the one its projection
    /// was taken over. Where it is at the shard's identity and watermark it is the answer, so a
    /// fragment evicted from the cache's memory tier is not opened and hashed again while a
    /// projection still maps it.
    ///
    /// Waits on a concurrent build of the same key for the wait budget, then yields
    /// [`EngineError::FragmentBuilding`].
    pub(crate) fn fragment_for(
        &self,
        session: &Session,
        shard: &crate::Shard,
        held: Option<&Arc<FrozenFragment>>,
    ) -> Result<Arc<FrozenFragment>> {
        if let Some(held) =
            held.filter(|held| held.identity == shard.identity && held.watermark == shard.watermark)
        {
            return Ok(Arc::clone(held));
        }
        shard
            .fragment(&self.fragments, &session.grant)
            .map_err(fragment_error)
    }

    /// Which layers this principal may know exist. A layer's label is evaluated against the
    /// credential's terms, the test an artifact's own label takes. A gate-failed name and a
    /// never-registered one answer identically, so a name outside this set reveals nothing about
    /// why.
    pub(crate) fn reachable_layers(&self, session: &Session) -> mosaica_lifecycle::ResolvedLayers {
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
        declaration: &mosaica_types::layer::LayerDeclaration,
    ) -> crate::artifacts::LabelGate<'s> {
        use mosaica_types::layer::MemberDefault;
        // A layer naming no field has no artifact labels, and its default says nothing.
        let unlabelled = !declaration.artifact_visibility.carries_own_labels()
            || match &declaration.artifact_visibility.default {
                MemberDefault::Inherited => true,
                MemberDefault::Label(label) => session.admits(&[label]),
            };
        match &session.held {
            Some(held) => crate::artifacts::LabelGate::new(&held.credentials, unlabelled),
            None => crate::artifacts::LabelGate::every_term(unlabelled),
        }
    }

    /// Which layers this principal may know exist, and which of those are currently served.
    ///
    /// Two questions, answered in that order. Reachability is resolved from the registry: one set
    /// probe, identical for a gate-failed name and a never-registered one. Whether a reachable
    /// layer is currently served is asked live, per call, against the overlay, since a suppression
    /// takes effect at the ack: a cache may bake in reachability but must never bake in whether a
    /// layer is served.
    pub fn visible_layers(&self, session: &Session) -> Vec<mosaica_types::layer::RegisteredLayer> {
        let generation = self.generation();
        self.reachable_layers(session)
            .names()
            .filter_map(|name| self.write.live().registered_layer(name))
            .filter(|layer| !generation.layer_denied(layer.space))
            .collect()
    }
}

fn fragment_error(e: FragmentCacheError) -> EngineError {
    match e {
        FragmentCacheError::Building => EngineError::FragmentBuilding,
        FragmentCacheError::Cancelled => EngineError::Cancelled,
        FragmentCacheError::Io(io_err) => EngineError::Io(io_err),
    }
}

/// The terms a credential presents. `auth_data` is the JSON `{"terms": ["<term>", ...]}`; each
/// term is held as [`mosaica_access::held_term`] says, and one it refuses is dropped. A
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
        held.extend(mosaica_access::held_term(term).map(str::to_owned));
    }
    held.sort_unstable();
    held.dedup();
    Ok(held)
}

/// The field a test may read directly. It lives here rather than in `crate::test_hooks` because
/// the field is private to this module.
impl Session {
    /// The keys resolved at authorise, and an empty set for a `read-all` session.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn satisfied_for_test(&self) -> &FxHashSet<TermId> {
        static NONE: std::sync::OnceLock<FxHashSet<TermId>> = std::sync::OnceLock::new();
        match &self.satisfied {
            Satisfied::Keys(keys) => keys,
            Satisfied::Every => NONE.get_or_init(FxHashSet::default),
        }
    }
}
