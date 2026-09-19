use super::*;

mod background;
mod commands;
mod fold;
mod publications;
mod values;

pub(super) use background::Background;

pub use commands::*;
pub(in crate::write) use fold::*;
pub(in crate::write) use publications::*;
use values::*;

// =================================================================================================
// The executor
// =================================================================================================

/// What `/control/ingest`'s batch id means to this executor: durably accepted, held in the open
/// commit window, or never seen. Computed by [`BatchState::of`] on the executor thread only.
pub(super) enum BatchState {
    /// Durably accepted: the WAL record is fsynced, the rows are applied and the ids are recorded.
    /// Same bytes replays these ids; different bytes is a `409`.
    Accepted {
        body_hash: [u8; 32],
        entity_ids: Vec<EntityId>,
    },
    /// Held in the open commit window, not yet acknowledged. Allocation happens at the close, so a
    /// byte-identical retry joins the queue of waiters the entry will ack.
    ///
    /// `window_seq` is the window the entry was found in; there is exactly one open window.
    Held {
        window_seq: u64,
        body_hash: [u8; 32],
    },
    /// Never seen. A new entry.
    ///
    /// Also what an accepted batch regresses to once WAL rotation reclaims its member: a retry with
    /// an `external_id` then 409s on the duplicate check, but one with none is re-ingested as a new
    /// entity. The horizon is about one flush; a client needing longer carries its own id column.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: the durable index, then the open window, then unknown. The two sets are
    /// disjoint: a batch id enters `accepted_batches` only at `close_window`. Durable is checked
    /// first because it is the half that survives a restart.
    pub(super) fn of<W>(live: &LiveState, window: &CommitWindow<W>, batch_id: &str) -> BatchState {
        if let Some((body_hash, entity_ids)) = live.accepted_batch(batch_id) {
            return BatchState::Accepted {
                body_hash,
                entity_ids,
            };
        }
        match window.held(batch_id) {
            Some((window_seq, body_hash)) => BatchState::Held {
                window_seq,
                body_hash,
            },
            None => BatchState::Unknown,
        }
    }
}

/// What [`Executor::admit_ingest`] did with a submission, as far as the drain loop needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// It is in the window (or its waiters).
    Admitted,
    /// It was answered outright (a replay, a join or a 409) and nothing was added to the window.
    Answered,
    /// A conflicting external id forced the open window to close. The pass must yield to
    /// `Executor::run`'s deny drain.
    YieldedAfterClose,
}

/// The most entries one deny window may hold, and the most changes `/control/changes` enqueues
/// before it collects. Bounds the drain so it terminates under sustained deny arrival. Raising it
/// reduces fsyncs and overlay clones at the cost of larger pending-receipt bursts.
pub const DENY_WINDOW_MAX_ENTRIES: usize = 1_000;

/// How many deny windows may pass before the overlay publishes regardless of whether the drain has
/// closed. A liveness floor: without it the newest manifest could trail live state indefinitely
/// under sustained deny arrival. Bounds the side-manifest lag to at most 64,000 dispositions,
/// already durable in the WAL and recovered by any restart.
pub(super) const OVERLAY_PUBLICATION_MAX_WINDOWS: u64 = 64;

/// How often the executor re-checks for a completed flush while one is in flight or completed but
/// not yet drained. A poll, not a push: the pool cannot hold the doorbell sender, since that would
/// block `WritePath::drop`'s join. Armed only while something is outstanding.
pub(super) const FLUSH_COMPLETION_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How long after a cycle failed to publish the next retry may come. Without this floor a failed
/// cycle would retry at [`FLUSH_COMPLETION_POLL`]'s rate. A period tick and a row trip are not held
/// back by it.
pub(super) const FAILED_CYCLE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// How often the executor looks for a completed fold while one is running. Coarser than
/// [`FLUSH_COMPLETION_POLL`] since a fold runs minutes to hours, not seconds. Once the fold has
/// sent, `fold_completed_pending` puts the wait back on the fast poll.
pub(super) const FOLD_COMPLETION_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// How long the executor waits before each re-attempt at making a deny window durable, and
/// therefore how many attempts there are: the first sync, plus one per entry here. The deny lane
/// is FIFO on a single thread, so this delay is paid by every deny queued behind a failing window.
/// Non-zero because an immediate retry cannot help a short-lived `ENOSPC`.
pub(super) const DENY_DURABILITY_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(200),
];

/// How many durability attempts one deny window gets in total: the original sync plus one per
/// [`DENY_DURABILITY_BACKOFF`] entry.
///
/// Public because a test observing the exhausted path must arm exactly this many failures.
pub const DENY_DURABILITY_ATTEMPTS: usize = DENY_DURABILITY_BACKOFF.len() + 1;

/// The levels a fold's retirement is about to move, and the set it retires.
///
/// A fold writes its manifest before it retires, since the retirement is not reversible: a
/// manifest that would not commit must leave it undone. [`Self::records`] composes a level's
/// records without the retired artifacts, and [`Self::version_after`] stamps them with the version
/// the level will carry once the retirement has run. A containment partition and a spatial level's
/// row forms are omitted for a pending level and recompose on first use.
pub(super) struct PendingRetirement {
    levels: Vec<(String, u32)>,
    retired: croaring::Bitmap,
}

impl PendingRetirement {
    pub(super) fn is_pending(&self, layer: &str, level: u32) -> bool {
        self.levels.iter().any(|(l, v)| l == layer && *v == level)
    }

    /// The version `layer`'s `level` will have once this fold's retirement has run.
    pub(super) fn version_after(&self, store: &ArtifactStore, layer: &str, level: u32) -> u64 {
        store.level_version(layer, level) + u64::from(self.is_pending(layer, level))
    }

    /// The level's records as the retirement will leave them: every artifact but those whose own
    /// entity is retired.
    pub(super) fn records<'s>(
        &'s self,
        store: &'s ArtifactStore,
        layer: &str,
        level: u32,
    ) -> impl Iterator<Item = (u32, &'s tessera_lifecycle::membership::ArtifactRecord)> + 's {
        let retired = &self.retired;
        store
            .level(layer, level)
            .filter(move |(_, record)| !retired.contains(record.entity.raw() as u32))
    }
}

/// How many ordinals a level's derived structures cover: one past the highest live ordinal,
/// which is the length the reader sizes the level at (`ArtifactRows::build_over`). A hole below
/// it is covered and a hole at the top is not.
pub(super) fn level_length<'a>(
    records: impl Iterator<Item = (u32, &'a tessera_lifecycle::membership::ArtifactRecord)>,
) -> u32 {
    records.map(|(ordinal, _)| ordinal + 1).max().unwrap_or(0)
}

/// What a fold hands the manifest commit in place of the held list: the derived files it has just
/// written, and the levels its retirement is about to move.
pub(super) struct FoldDerived<'a> {
    written: &'a [tessera_store::manifest::DerivedExtent],
    pending_retirement: &'a [(String, u32)],
}

/// Every level's version, and the derived files of `held` stamped with their level's version.
///
/// A file whose level has moved is dropped, so a manifest never names one nothing could adopt. For
/// a level in `pending_retirement` the version is the store's plus one: what the level will carry
/// once the retirement has run.
pub(super) fn artifact_coordinates(
    store: &ArtifactStore,
    held: &[tessera_store::manifest::DerivedExtent],
    pending_retirement: &[(String, u32)],
) -> (
    Vec<tessera_store::manifest::LevelVersion>,
    Vec<tessera_store::manifest::DerivedExtent>,
) {
    let expected = |layer: &str, level: u32| {
        let pending = pending_retirement
            .iter()
            .any(|(l, v)| l == layer && *v == level);
        store.level_version(layer, level) + u64::from(pending)
    };
    let versions = store
        .level_versions()
        .map(|(layer, level, _)| tessera_store::manifest::LevelVersion {
            layer: layer.to_string(),
            level,
            version: expected(layer, level),
        })
        .collect();
    let still_true = held
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    (versions, still_true)
}

/// The fold-written files whose stamped version is the level's now, after the fold's retirement
/// has run; every other one is dropped and named. See [`PendingRetirement`].
pub(super) fn held_at_current_version(
    store: &ArtifactStore,
    entries: &[tessera_store::manifest::DerivedExtent],
) -> Vec<tessera_store::manifest::DerivedExtent> {
    entries
        .iter()
        .filter(|entry| {
            let now = store.level_version(&entry.layer, entry.level);
            if now == entry.level_version {
                return true;
            }
            tracing::error!(
                layer = %entry.layer,
                level = entry.level,
                form = entry.form.dir(),
                stamped = entry.level_version,
                now,
                "ALARM: a fold-written derived file is stamped with a version the level does not \
                 carry after the retirement; it is dropped and the level recomposes on first use"
            );
            false
        })
        .cloned()
        .collect()
}

/// Why [`Executor::commit_side_manifest`] did not commit: the manifest would regress durable
/// state, or the store could not write it. One type so every publication site's failure arm
/// reports whichever it was through the `error = %e` it already has.
pub(super) enum ManifestCommitRefused {
    Regresses(crate::geometry::ManifestRegression),
    Store(tessera_store::StoreError),
}

impl std::fmt::Display for ManifestCommitRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestCommitRefused::Regresses(r) => r.fmt(f),
            ManifestCommitRefused::Store(e) => e.fmt(f),
        }
    }
}

/// Replace a manifest's deny fields with the overlay's live state. Serialised fresh at every
/// write, never carried forward: copying an earlier manifest's fields forward would leave an
/// unsuppress never reaching disc. The two fields are taken from the two bitmaps separately, never
/// from `Overlay::denied`'s union, since publishing the union would make every deletion look
/// retirable by an unsuppress.
pub(super) fn write_deny_state(manifest: &mut SegmentsManifest, overlay: &Overlay) {
    manifest.deny = overlay
        .suppressed_entities()
        .into_iter()
        .map(|entity_id| ManifestDenyEntry {
            entity_id,
            cause: "suppress".to_string(),
        })
        .collect();
    manifest.tombstones = overlay.deleted_entities();
}

/// Carry the live vocabulary bindings into a manifest's `vocabulary_extensions`,
/// `write_deny_state`'s sibling, called beside it at every publication site except the fold's.
/// Union, never restate: a binding must never shrink, so this appends only what
/// `extensions_beyond` gives beyond what the manifest already carries. The fold does not call
/// this: it folds every served extension into `MANIFEST.vocabularies` directly.
pub(super) fn write_vocabulary_extensions(
    manifest: &mut SegmentsManifest,
    vocabularies: &Vocabularies,
    bundle_vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) {
    for extension in vocabularies.extensions_beyond(bundle_vocabularies) {
        match manifest
            .vocabulary_extensions
            .iter_mut()
            .find(|held| held.name == extension.name)
        {
            Some(held) => {
                for value in extension.values {
                    if !held.values.iter().any(|v| v.key == value.key) {
                        held.values.push(value);
                    }
                }
            }
            None => manifest.vocabulary_extensions.push(extension),
        }
    }
}

#[cfg(test)]
mod vocabulary_extensions_tests {
    use super::*;
    use tessera_store::manifest::{
        ManifestVocabulary, ManifestVocabularyValue, VocabularyExtension, VocabularyKind,
    };

    pub(super) fn empty_vocabulary(name: &str) -> ManifestVocabulary {
        ManifestVocabulary {
            name: name.to_string(),
            kind: VocabularyKind::Discovered,
            visibility: crate::Visibility::Derived,
            width: tessera_spatial::tiler::ScalarType::U32,
            values: Vec::new(),
            reserved: Vec::new(),
        }
    }

    /// A carried binding must survive even when the live view has nothing to say about it: a write
    /// touching an unrelated vocabulary must not erase an extension already held.
    #[test]
    pub(super) fn a_carried_extension_survives_a_write_the_live_view_recomputes_nothing_for() {
        let mut manifest = SegmentsManifest::empty();
        manifest.vocabulary_extensions.push(VocabularyExtension {
            name: "legacy".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "held".to_string(),
                code: 7,
                title: None,
            }],
        });

        let vocabularies = Vocabularies::seed(&[empty_vocabulary("department")], &[], &[]).unwrap();

        write_vocabulary_extensions(&mut manifest, &vocabularies, &[]);

        let legacy = manifest
            .vocabulary_extensions
            .iter()
            .find(|e| e.name == "legacy")
            .expect("a binding this manifest already carried must not be dropped");
        assert_eq!(legacy.values.len(), 1);
        assert_eq!(legacy.values[0].key, "held");
        assert_eq!(legacy.values[0].code, 7);
    }

    /// A fresh mint is appended beside what is already carried, and a restated binding is not duplicated.
    #[test]
    pub(super) fn a_fresh_binding_is_appended_beside_what_is_already_carried_and_not_duplicated() {
        let mut manifest = SegmentsManifest::empty();
        manifest.vocabulary_extensions.push(VocabularyExtension {
            name: "department".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "eng".to_string(),
                code: 4,
                title: None,
            }],
        });

        let mut vocabularies =
            Vocabularies::seed(&[empty_vocabulary("department")], &[], &[]).unwrap();
        vocabularies
            .get_mut("department")
            .unwrap()
            .seed_value("eng", 4)
            .unwrap();
        vocabularies
            .get_mut("department")
            .unwrap()
            .mint("finance")
            .unwrap();

        write_vocabulary_extensions(&mut manifest, &vocabularies, &[]);

        let department = manifest
            .vocabulary_extensions
            .iter()
            .find(|e| e.name == "department")
            .unwrap();
        let mut keys: Vec<&str> = department.values.iter().map(|v| v.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["eng", "finance"],
            "the carried key and the fresh one both survive, each exactly once"
        );
    }
}

/// The one plan a dispatch sends, chosen by oldest unflushed row. Free and pure so the choice can
/// be tested without an executor. See `Executor::dispatch_flushes` for why one plan.
pub(super) fn plan_to_dispatch(
    plans: Vec<(String, crate::flush::FlushPlan)>,
) -> Option<(String, crate::flush::FlushPlan)> {
    plans.into_iter().min_by_key(|(_, plan)| {
        // `items` is ascending by entity id, so the first is this view's oldest waiting row. An
        // empty plan cannot occur (`plan_flush` returns `NothingToFlush`); sorting it last keeps a
        // hypothetical one from winning every tick.
        plan.items
            .first()
            .map_or(u64::MAX, |(entity, _)| entity.raw())
    })
}

/// Whether a completed flush's dictionary moved under it. See the call site in
/// [`Executor::publish_flush`].
///
/// A flush that promoted nothing (`None`) is never discarded however far the dictionary has moved:
/// its tier names only ordinals below the length it planned against, which append-only extension
/// preserves.
pub(super) fn dictionary_moved_under(promoted_from_dict_len: Option<u32>, live_len: u32) -> bool {
    promoted_from_dict_len.is_some_and(|planned| planned != live_len)
}

/// One superseded prefix awaiting reclamation, and the two `Arc`s whose release says no thread can
/// still resolve a path inside it. See [`Executor::pending_reclaim`].
pub(super) struct PendingReclaim {
    generation: Arc<Generation>,
    prefix_dir: PathBuf,
    /// Every sidecar that was live over this prefix before the one the held generation carries.
    /// See [`Executor::superseded_sidecars`].
    superseded_sidecars: Vec<std::sync::Weak<crate::session::ExternalIdIndex>>,
}

/// Seconds since the Unix epoch, or `None` if the clock is before it.
///
/// `None` reads as "no fold has ended yet", switching the interval floor off rather than jamming it
/// on: the safe direction, and the same answer a fresh process gives.
pub(super) fn unix_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// The `(layer, level)` a record changes the artifacts of, and `None` for every other record:
/// what a caller needs to read that level's version before the record moves it.
pub(super) fn artifact_level_of(record: &WalRecord) -> Option<(&str, u32)> {
    match record {
        WalRecord::ArtifactPublish { layer, level, .. }
        | WalRecord::ArtifactGrow { layer, level, .. }
        | WalRecord::ArtifactFill { layer, level, .. } => Some((layer.as_str(), *level)),
        _ => None,
    }
}

/// The growth records one closed window owes, with the index of the entry to blame if an append
/// fails, in the order they are to be appended.
///
/// One record per `(layer, level)` for the whole window, not one per entry: several batches naming
/// one cluster merge into a union. A join still carries its own `(layer, level, ordinal)`.
///
/// The entities are `entity_ids[row]`, the assignment this window just made, in the caller's own
/// row order.
pub(super) fn growth_records<W>(closed: &[tessera_lifecycle::ClosedEntry<W>]) -> Vec<(WalRecord, usize)> {
    use std::collections::BTreeMap;
    /// One `(layer, level)`'s joins: the entry to blame for the append, and a bitmap per ordinal.
    pub(super) type Level = (usize, BTreeMap<u32, croaring::Bitmap>);
    // Ordered, so replay order does not depend on hash iteration: two nodes replaying one log must
    // read the same sequence.
    let mut by_level: BTreeMap<(&str, u32), Level> = BTreeMap::new();
    for (index, entry) in closed.iter().enumerate() {
        for join in &entry.memberships {
            // A key with no ordinal was minted at the close, and a minted artifact was published
            // carrying these rows: one record instead of a publication and a growth against it.
            let Some(ordinal) = join.ordinal else {
                continue;
            };
            let (_, ordinals) = by_level
                .entry((join.layer.as_str(), join.level))
                .or_insert_with(|| (index, BTreeMap::new()));
            let joining = ordinals.entry(ordinal).or_default();
            for row in &join.rows {
                let entity = entry.entity_ids[*row as usize];
                // Entity space is `u32`-wide, so the narrowing is total.
                joining.add(entity.raw() as u32);
            }
        }
    }
    by_level
        .into_iter()
        .filter_map(|((layer, level), (index, ordinals))| {
            let joins = ordinals
                .iter()
                .map(|(ordinal, joining)| (*ordinal, joining));
            tessera_lifecycle::membership::growth_record(layer, level, joins)
                .map(|record| (record, index))
        })
        .collect()
}

/// The artifacts a closed window's rows named and no artifact holds: the records that create them,
/// in the order they must be appended, and how many each entry is to be told it created.
///
/// Minting happens here, at the close, not at admission: an ordinal is claimed from the level's own
/// cursor and is durable only in the record that claims it. One artifact per key per level for the
/// whole window, re-resolved against `ArtifactStore::ordinal_of_key` in case a publication landed
/// since admission, in which case it grows instead of minting. A minted artifact is published
/// carrying its members, so `growth_records` skips a membership whose ordinal is `None`. This is
/// the one route by which the wire creates a lineage edge: a growth never creates one, so the edge
/// arrives parent before child.
pub(super) fn mint_plan<W>(
    closed: &[tessera_lifecycle::ClosedEntry<W>],
) -> Option<(MintPlan, Vec<tessera_lifecycle::BatchEdge>)> {
    use std::collections::BTreeMap;
    let mut wanted: MintPlan = BTreeMap::new();
    for (index, entry) in closed.iter().enumerate() {
        for join in &entry.memberships {
            if join.ordinal.is_some() {
                continue;
            }
            let (_, members) = wanted
                .entry((join.layer.clone(), join.level, join.key.clone()))
                // The first entry that named the key owns the mint.
                .or_insert_with(|| (index, croaring::Bitmap::new()));
            for row in &join.rows {
                // Entity space is `u32`-wide, so the narrowing is total.
                members.add(entry.entity_ids[*row as usize].raw() as u32);
            }
        }
    }
    if wanted.is_empty() {
        return None;
    }
    let edges = closed
        .iter()
        .flat_map(|e| e.edges.iter().cloned())
        .collect();
    Some((wanted, edges))
}

/// What one window is about to mint: `(layer, level, key)` → the entry that first named it, and the
/// entities joining it.
pub(super) type MintPlan = std::collections::BTreeMap<(String, u32, String), (usize, croaring::Bitmap)>;

/// A key that acquired an artifact between its resolution and its preparation grows into it rather
/// than minting a second one.
///
/// [`Executor::prepare_mints`] re-resolves every key against the store and answers the ones that
/// turned out held; this writes those ordinals back onto the memberships, so `growth_records`
/// carries them as ordinary joins. A membership left with no ordinal is one the preparation is
/// about to mint. Only the ingest door can find something here, since a window can stay open across
/// a `PublishArtifacts` command between admission and close; at the values door `resolved` is
/// always empty.
pub(super) fn settle_resolved_ordinals(
    memberships: &mut [tessera_lifecycle::ResolvedMembership],
    resolved: &std::collections::BTreeMap<(String, u32, String), u32>,
) {
    if resolved.is_empty() {
        return;
    }
    for join in memberships.iter_mut() {
        if join.ordinal.is_some() {
            continue;
        }
        let at = (join.layer.clone(), join.level, join.key.clone());
        join.ordinal = resolved.get(&at).copied();
    }
}

/// What [`Executor::prepare_mints`] answers: the publication records to append in order, the keys
/// that turned out to be held after all and the ordinal each resolved to, and the keys this run
/// created. `Err` is the refusal text the caller's waiters are answered with.
pub(super) type PreparedMints = Result<
    (
        Vec<WalRecord>,
        std::collections::BTreeMap<(String, u32, String), u32>,
        std::collections::BTreeSet<(String, u32, String)>,
    ),
    String,
>;

/// How often a degraded node retries WAL recovery. A second is short against the interval an
/// operator would take to notice, and long enough that a genuinely dead device is retried sixty
/// times a minute rather than continuously.
///
/// Not a latency bound on anything a caller sees: a degraded node still answers denies immediately.
pub(super) const WAL_RECOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One deny in an open window: its record, and everything needed to apply it and answer its caller.
///
/// `record` is built at the drain rather than at the append so the window is a list of things that
/// are ready to be written: the append loop does no work that can be got wrong per entry.
pub(super) struct DenyEntry {
    record: WalRecord,
    entity: EntityId,
    op: ChangeOp,
    /// The waiter, or `None` for a cascaded deletion (`Executor::cascade_dependents`), which has no
    /// caller to answer but is otherwise an ordinary entry: its own WAL record, applied in the same
    /// window, retired at the same fold.
    reply: Option<Reply<()>>,
}

/// The single writer. One per partition, on its own thread, owning the WAL by value.
pub(super) struct Executor {
    pub(super) wal: ExecutorWal,
    pub(super) live: Arc<LiveState>,
    /// The only publishing capability in the write path. Not in [`LiveState`], which the handler
    /// side shares.
    pub(super) generation: Arc<GenerationHandle>,
    /// The row-projection cache, pruned of generations older than the retention depth at the swap.
    pub(super) row_projection_cache: Arc<RowProjectionCache>,
    /// See [`MaintenanceDeps::region_cache`].
    pub(super) region_cache: Arc<
        crate::single_flight::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// The artifact row forms, rebuilt here at the fold, and read by every viewport.
    pub(super) artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// See [`MaintenanceDeps::shapes`].
    pub(super) shapes: Arc<crate::shapes::ShapeStore>,
    /// The lineages, rebuilt beside them and for the same reason.
    pub(super) lineages: Arc<crate::cut::Lineages>,
    /// The supplied-content tables. Not warmed at the fold: a level is merely stale after one, and
    /// the first request that wants it pays to read it.
    pub(super) level_contents: Arc<crate::artifact_content::LevelContents>,
    pub(super) queues: LifecycleQueues,
    pub(super) health: Arc<ExecutorHealth>,
    /// The last window's sequence number; all it has to be is distinct per window.
    pub(super) window_seq: u64,
    /// `flush_max_age_secs`, the tick's period.
    pub(super) flush_max_age_secs: u64,
    /// `flush_max_items`, buffered rows at which the tick comes due ahead of its period.
    pub(super) flush_max_items: usize,
    /// The next `SEGMENTS-<n>.json` number, taken at the moment a writer writes rather than when a
    /// flush is planned. [`Executor::allocate_manifest_n`] also raises it over the files on disc.
    pub(super) next_manifest_n: u64,
    /// Whether live state holds something no side-manifest carries yet.
    pub(super) deny_dirty: bool,
    /// Deny windows applied since the last publication, the counter
    /// [`OVERLAY_PUBLICATION_MAX_WINDOWS`] floors.
    pub(super) windows_since_publication: u64,
    /// The bundle root, not the prefix directory: a fold moves the prefix, so
    /// [`Executor::prefix_dir`] derives it at each use.
    pub(super) bundle_root: PathBuf,
    pub(super) identity_key: IdentityKey,
    /// The shared compute pool a flush executes on.
    pub(super) pool: Arc<rayon::ThreadPool>,
    /// See [`MaintenanceDeps::max_distinct_terms`].
    pub(super) max_distinct_terms: u64,
    /// The entity-space coalesce's policy, in-flight flag, attempt counter and completion channel:
    /// separate from a flush so the cheap one does not wait on the expensive one.
    pub(super) coalesce_policy: crate::coalesce::CoalescePolicy,
    /// The entity-space coalesce.
    pub(super) coalesce: Background<crate::coalesce::CompletedCoalesce>,
    /// The background refresh's dependencies. See [`crate::refresh`].
    pub(super) refresh: crate::refresh::RefreshDeps,
    /// The row-space merge's policy, in-flight flag, attempt counter and completion channel:
    /// separate from the flush and the coalesce, since a merge publishes its own swap.
    pub(super) coalesce_enabled: Arc<AtomicBool>,
    pub(super) merge_policy: MergePolicy,
    pub(super) merge_enabled: Arc<AtomicBool>,
    /// The row-space merge.
    pub(super) merge: Background<crate::merge::CompletedMerge>,
    /// The compaction fold. It runs on its own thread, not the shared pool: it takes minutes to
    /// hours and the pool serves viewports.
    pub(super) fold: Background<crate::compact::CompletedFold>,
    /// The suggestion index's rebuild. No plan, no gate, nothing to refuse: it reads a vocabulary
    /// out of the generation and writes files the manifest does not name.
    pub(super) suggest_dir: PathBuf,
    /// The suggestion-index rebuild.
    pub(super) suggest: Background<crate::suggest::CompletedSuggest>,
    /// The flush. Its in-flight flag is [`ExecutorHealth::flush_in_flight`], which status reads.
    pub(super) flush: Background<crate::flush::CompletedFlush>,
    /// See [`MaintenanceDeps::configured_merge_bytes`].
    pub(super) configured_merge_bytes: Option<u64>,
    /// See [`MaintenanceDeps::fold_paused`].
    pub(super) fold_paused: Arc<AtomicBool>,
    /// See [`MaintenanceDeps::fold_publication_paused`].
    pub(super) fold_publication_paused: Arc<AtomicBool>,
    /// See [`MaintenanceDeps::merge_publication_paused`].
    pub(super) merge_publication_paused: Arc<AtomicBool>,
    /// See [`crate::compact::CompactionSchedule`]. Consulted at the tick, beside the flush's own.
    pub(super) compaction: crate::compact::CompactionSchedule,
    /// When the last fold attempt started, as a unix second. Stamped by every dispatch whatever the
    /// attempt then does, so the interval limits attempts.
    pub(super) last_fold_start_unix: Option<u64>,
    /// Every external-id sidecar replaced over the live prefix, weakly held: a `Weak` answers
    /// whether one is still alive without keeping its mappings alive itself. Moved into
    /// [`PendingReclaim`] at a fold.
    pub(super) superseded_sidecars: Vec<std::sync::Weak<crate::session::ExternalIdIndex>>,
    /// Every membership extent this node has published: the complete list, not a diff, since a
    /// publication clones a manifest that may be stale and extending that clone would drop entries.
    pub(super) membership_extents: Vec<tessera_store::manifest::MembershipExtent>,
    /// Every derived file the current prefix holds. What reaches a manifest is this list filtered
    /// to the files the store's level versions still make adoptable ([`artifact_coordinates`]); a
    /// fold replaces it wholesale.
    pub(super) derived_extents: Vec<tessera_store::manifest::DerivedExtent>,
    /// Every artifact content extent, held and written like `membership_extents`, which it travels
    /// with: a membership without its content withholds the artifact.
    pub(super) artifact_record_extents: Vec<tessera_store::manifest::RecordExtent>,
    /// Superseded prefixes awaiting reclamation, each held by the generation that named it. A
    /// prefix is deleted only once nothing else holds that generation or its external-id sidecar. A
    /// process that exits first leaves the tree for the startup sweep.
    pub(super) pending_reclaim: Vec<PendingReclaim>,
    /// When the last tick fired. Started at construction, so the first tick is one period after
    /// the executor starts rather than immediately at startup.
    pub(super) last_tick: std::time::Instant,
    /// What every accepted write since the last tick did to each level's row forms, applied at the
    /// next tick. One level's deltas carry consecutive level versions.
    pub(super) pending_forms: std::collections::BTreeMap<(String, u32), Vec<crate::artifacts::LevelDelta>>,
    /// The WAL's sequence position after the last rotation, so a tick can tell whether the log has
    /// grown since: the deny-only regime's rotation trigger.
    pub(super) wal_position_at_last_rotation: u64,
    /// When [`Executor::sample_wal_gauge`] last began a walk, or `None` before the first one.
    pub(super) last_wal_sample: Option<std::time::Instant>,
    /// Walks taken, published as [`WalGauge::samples`] so a reader can tell a refreshed reading
    /// from one the rate limit held back.
    pub(super) wal_samples: u64,
    #[cfg(feature = "fault-injection")]
    pub(super) faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

/// The parent each child in these edges is named under, refusing a child named under two.
///
/// The child is keyed by its own level, which a levelled taxonomy needs: one key legitimately sits
/// at two levels and carries a different parent at each.
pub(super) fn parent_of_each_child(
    edges: &[tessera_lifecycle::BatchEdge],
) -> Result<std::collections::BTreeMap<(&str, u32, &str), &str>, String> {
    let mut claimed: std::collections::BTreeMap<(&str, u32, &str), &str> = Default::default();
    for edge in edges {
        let at = (edge.layer.as_str(), edge.level, edge.child.as_str());
        if let Some(first) = claimed.insert(at, edge.parent.as_str()) {
            if first != edge.parent {
                return Err(format!(
                    "{} in level {} of {} is named as a child of both {first} and {}; a list \
                     column declares the edges, so name one parent for the child or publish the \
                     two hierarchies as separate layers",
                    edge.child, edge.level, edge.layer, edge.parent
                ));
            }
        }
    }
    Ok(claimed)
}

impl Executor {
    /// Drop the region decompositions of generations older than the retention depth: the same
    /// pass, at the same swap, as `RowProjectionCache::prune_generations_below`.
    pub(super) fn prune_region_cache(&self, segments_version: u64) {
        let floor = segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS);
        self.region_cache
            .retain_keys(|key| key.segments_version >= floor);
    }

    /// Drain deny to empty, then execute at most one work item, then repeat, blocking only once
    /// both queues have been observed empty.
    ///
    /// [`Executor::run_work_pass`] returns as soon as it closes a window, so a deny's wait is
    /// bounded by the window in front of it, not by queue depth. A deny may overtake a queued
    /// ingest safely: an item is established only at apply, so append order still equals apply
    /// order. Shutdown drains and executes rather than discarding.
    pub(super) fn run(&mut self) {
        self.sample_wal_gauge();
        loop {
            self.recover_wal();
            // Applied before the tick plans another, or it would re-plan rows already written.
            let published = self.publish_completed_flushes()
                | self.publish_completed_coalesces()
                | self.publish_completed_merges()
                | self.publish_completed_folds()
                | self.publish_completed_suggests();
            self.tick_if_due();
            while self.run_deny_pass() {}
            self.publish_overlay_state();
            if self.run_work_pass() || published {
                continue;
            }
            if !self.wait_for_work() {
                break;
            }
        }
    }

    /// The flush tick: the one cadence on which geometry is published.
    ///
    /// Runs at the top of the loop, before the deny drain, so a tick is never delayed by work that
    /// arrived after it came due, and after the drain, so a tick that publishes does not preempt a
    /// deny already queued. Three triggers reach this cadence and none publishes off it: the
    /// period, the buffered-row count, and `POST /control/flush`. It also drives `reclaim`.
    pub(super) fn tick_if_due(&mut self) {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        let rows_due = self.health.buffered_items.load(Ordering::SeqCst) >= self.flush_max_items;
        let period_due = self.last_tick.elapsed() >= period;
        let due = period_due || rows_due;
        let requested = self.health.flush_requested.load(Ordering::SeqCst);
        let fold_requested = self.health.fold_requested.load(Ordering::SeqCst);
        if !due && !requested && !fold_requested {
            return;
        }
        // Floored ([`FAILED_CYCLE_RETRY`]) so a retry does not re-plan the buffer at the
        // completion-poll rate.
        if !due && self.health.failed_cycle_backoff().is_some() {
            return;
        }
        let flush_in_flight = self.flush.in_flight();
        if !flush_in_flight {
            self.health.open_publication_cycle();
        }
        self.sample_wal_gauge();
        // Ahead of the flush's in-flight gate: these wait on nothing this executor does.
        self.reclaim_superseded_prefixes();
        self.dispatch_suggest_rebuild();
        self.publish_row_forms();

        let generation = self.generation.load_full();

        // A period tick arriving while a flush runs is skipped, not queued. A requested flush is
        // not consumed by a skip: the flag stays armed for the first iteration after it lands.
        if flush_in_flight {
            if due {
                self.last_tick = std::time::Instant::now();
                self.health.mark_tick(self.last_tick);
                self.health.ticks.fetch_add(1, Ordering::Relaxed);
                let flushable = generation
                    .buffer
                    .iter()
                    .filter(|(entity, _)| !generation.overlay.is_deleted(**entity))
                    .count();
                self.health
                    .flushable_items
                    .store(flushable, Ordering::SeqCst);
                // A missed period is the visibility-latency breach `flush_max_age_secs` guards.
                if flushable > 0 && period_due {
                    self.health.flush_skips.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "ALARM: a flush was still running when the next tick came due, so this \
                         tick published nothing. The effective publication period is longer \
                         than flush_max_age_secs, which is a visibility-latency breach"
                    );
                }
            }
            return;
        }

        self.last_tick = std::time::Instant::now();
        self.health.mark_tick(self.last_tick);
        self.health.ticks.fetch_add(1, Ordering::Relaxed);

        let mark = StageMark::now();
        let mut flushable = 0usize;
        let mut gated = false;
        let mut plans: Vec<(String, crate::flush::FlushPlan)> = Vec::new();
        for view in views_of(&generation) {
            match crate::flush::plan_flush(
                &generation,
                &view,
                self.wal.is_poisoned(),
                self.health.overlay_diverged.load(Ordering::SeqCst),
            ) {
                Ok(plan) => {
                    flushable += plan.items.len();
                    plans.push((view, plan));
                }
                Err(crate::flush::NoFlush::NothingToFlush) => {}
                Err(refusal) => {
                    gated = true;
                    // Once per period: the refusal stands until an operator acts.
                    if self.health.refusal_log_due() {
                        tracing::warn!(
                            view = %view,
                            gate = ?refusal,
                            "flush skipped: this node publishes no geometry in this state"
                        );
                    }
                }
            }
        }
        self.health.flush_lap(crate::flush::FlushStage::Plan, mark);
        self.health
            .flushable_items
            .store(flushable, Ordering::SeqCst);

        if plans.is_empty() {
            self.health.deferred_plans.store(false, Ordering::SeqCst);
            self.rotate_if_grown();
            if gated {
                self.note_publication_failure();
            } else {
                self.health.close_publication_cycle();
            }
        } else if !self.dispatch_flushes(&generation, plans) {
            self.note_publication_failure();
        }
        // Dispatched before the two it suspends, so a tick that starts a fold does not also start
        // a merge that the flip would orphan.
        self.dispatch_fold(&generation);
        self.dispatch_coalesce(&generation);
        self.dispatch_merge(&generation);
        drop(generation);
    }

    /// Record that this cycle published nothing it was asked to publish: the cycle stays open, the
    /// request is re-armed, and the retry is floored.
    pub(super) fn note_publication_failure(&self) {
        self.health.fail_publication_cycle();
    }

    /// Whether this executor may still write durable state: the two latching postures, asked in
    /// one place so a new publication kind cannot miss one.
    ///
    /// Does not include the WAL's poison flag: that one is recoverable and is asked separately by
    /// the callers that care. These two are terminal until a restart.
    pub(super) fn may_publish(&self) -> bool {
        !self.health.overlay_diverged.load(Ordering::SeqCst)
            && !self.health.prefix_diverged.load(Ordering::SeqCst)
    }

    /// If the WAL is degraded and the degradation is one a discard can end, end it.
    ///
    /// By the time this runs every caller has been told its write is not durable, so making those
    /// bytes durable afterwards would be fail-open: an exhausted deny window's `unsuppress` would
    /// undo a suppression the operator was told still stood. The region is discarded instead. A
    /// torn append does not recover: such a node stays `WalPoisoned` until restarted.
    pub(super) fn recover_wal(&mut self) {
        if !self.wal.is_poisoned() {
            return;
        }
        if !self.wal.is_recoverable() {
            return;
        }
        // Deliberately silent about failing: a log line per attempt would turn one storage fault
        // into an unbounded stream of them.
        if self.wal.discard_undurable().is_ok() {
            // The discard did not un-apply anything: every deletion and suppression applied under
            // the apply-anyway rule is in force in memory with no record behind it.
            if !self.health.overlay_diverged.swap(true, Ordering::SeqCst) {
                tracing::error!(
                    "ALARM: this node recovered its WAL in process, so its overlay now holds \
                     dispositions no durable record backs. It keeps serving and keeps applying \
                     denies, but publishes NO flush and rotates NO WAL until restarted; ingest \
                     stops becoming visible. Restart this node."
                );
            }
        }
        self.observe_wal();
    }

    /// Block until something may be waiting, and report whether the executor should keep running.
    ///
    /// While the WAL is degraded this also wakes on a timer, since `/readyz` steers traffic away
    /// from a degraded node and recovery would otherwise be reachable only by traffic. Shutdown
    /// leaves only from here, after both queues have been observed empty: a timeout resumes the
    /// loop, and only a disconnect ends it.
    pub(super) fn wait_for_work(&self) -> bool {
        // Bounded by the next tick, always, so a quiescent node still runs reclaim.
        let until_tick = std::time::Duration::from_secs(self.flush_max_age_secs)
            .saturating_sub(self.last_tick.elapsed());
        let wait = if self.wal.is_poisoned() {
            until_tick.min(WAL_RECOVERY_POLL_INTERVAL)
        } else if let Some(backoff) = self.health.failed_cycle_backoff() {
            until_tick.min(backoff)
        } else if self.health.flush_requested.load(Ordering::SeqCst)
            || self.flush.outstanding()
            || self.coalesce_outstanding()
            || self.merge_outstanding()
            || self.fold.completed_pending()
        {
            // The pool cannot ring the doorbell (see `flush_submit`).
            until_tick.min(FLUSH_COMPLETION_POLL)
        } else if self.fold.in_flight() {
            // Coarser, since a fold runs minutes to hours, not seconds.
            until_tick.min(FOLD_COMPLETION_POLL)
        } else {
            until_tick
        };
        !matches!(
            self.queues.bell.recv_timeout(wait),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        )
    }

    /// The deny window: gather the queued denies into one committable unit and commit it. Returns
    /// whether anything was found, which is what keeps [`Executor::run`] draining before it blocks.
    /// Amortises an item-at-a-time path's per-entry fsync and full [`Overlay`] clone (which shrinks
    /// only at a fold, so an N-item revocation would otherwise copy Θ(N²) entries).
    ///
    /// The window closes when the queue is observed empty or [`DENY_WINDOW_MAX_ENTRIES`] entries
    /// are reached, checked inside the drain since every entry pulled is one a concurrent submitter
    /// can replace. No linger: the deny lane stays unbounded and drained to empty before any work.
    pub(super) fn run_deny_pass(&mut self) -> bool {
        let mut entries: Vec<DenyEntry> = Vec::new();

        while entries.len() < DENY_WINDOW_MAX_ENTRIES {
            let Ok(command) = self.queues.deny.try_recv() else {
                break;
            };
            let Command::Change { entity, op, reply } = command else {
                // Only a `Change` rides the deny queue; this arm applies immediately, so the
                // window gathered so far is committed first to keep append order equal to apply
                // order.
                if !entries.is_empty() {
                    self.commit_denies(std::mem::take(&mut entries));
                }
                self.execute(command);
                return true;
            };
            entries.push(DenyEntry {
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op,
                },
                entity,
                op,
                reply: Some(reply),
            });
        }

        if entries.is_empty() {
            return false;
        }
        self.cascade_dependents(&mut entries);
        self.commit_denies(entries);
        true
    }

    /// Add a deletion for every artifact that depends on one this window deletes.
    ///
    /// A cascaded deletion is an ordinary entry: its own `ChangeByEntity` record in the same
    /// append, applied to the same overlay clone, retired at the same fold. Added before the
    /// append, so a restart rebuilds the same cascade from the log rather than re-deriving it. Only
    /// `Delete` cascades: a suppressed dependent is withheld by the serving predicate instead.
    pub(super) fn cascade_dependents(&mut self, entries: &mut Vec<DenyEntry>) {
        let deleted: Vec<EntityId> = entries
            .iter()
            .filter(|e| matches!(e.op, ChangeOp::Delete))
            .map(|e| e.entity)
            .collect();
        if deleted.is_empty() {
            return;
        }
        let cascade = self
            .live
            .with_artifacts(|store| store.cascade_from(&deleted));
        for entity in cascade {
            entries.push(DenyEntry {
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op: ChangeOp::Delete,
                },
                entity,
                op: ChangeOp::Delete,
                reply: None,
            });
        }
    }

    /// `append × k → one fsync → apply → one swap → ack × k`, with the apply-anyway exception for
    /// deny ops folded per entry. Append order is entries order is apply order, so a `suppress D`
    /// and a later `unsuppress D` in the same window resolve as they would have as two commands.
    ///
    /// On an unrepaired append or fsync failure, every [`ChangeOp::Delete`] and
    /// [`ChangeOp::Suppress`] in the window is applied anyway, hiding the items immediately, and
    /// every waiter still gets an error; every [`ChangeOp::Unsuppress`] applies nothing. Replay
    /// discards every record the window appended, so the applied `Suppress` comes back unhidden on
    /// restart: durability was owed and not reached, and the caller must retry. The item stays
    /// hidden in memory until then, and the node stops claiming readiness.
    pub(super) fn commit_denies(&mut self, entries: Vec<DenyEntry>) {
        let mut failed_at: Option<(usize, WalError)> = None;
        for (i, entry) in entries.iter().enumerate() {
            if let Err(e) = self.wal.append(&entry.record) {
                failed_at = Some((i, e));
                break;
            }
        }
        // One fsync for the whole window. Every entry is durable when it returns, or none is.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                if let Err(e) = self.retry_deny_durability(&entries, e) {
                    failed_at = Some((0, e));
                }
            }
        }
        self.observe_wal();

        if let Some((index, error)) = failed_at {
            let applied: Vec<(EntityId, ChangeOp)> = entries
                .iter()
                .filter(|e| matches!(e.op, ChangeOp::Delete | ChangeOp::Suppress))
                .map(|e| (e.entity, e.op))
                .collect();
            if !applied.is_empty() {
                // Deliberately does not mark the overlay dirty: publishing these would make a
                // never-acked deny permanent, since no durable record backs them.
                self.apply_changes(applied);
            }
            let mut real = Some(error);
            for (i, entry) in entries.into_iter().enumerate() {
                let e = if i == index {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                if let Some(reply) = &entry.reply {
                    reply.fail(ExecError::Wal(e));
                }
            }
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);

        let applied: Vec<(EntityId, ChangeOp)> = entries.iter().map(|e| (e.entity, e.op)).collect();

        self.apply_changes(applied);
        self.deny_dirty = true;
        self.windows_since_publication += 1;
        if self.windows_since_publication >= OVERLAY_PUBLICATION_MAX_WINDOWS {
            self.publish_overlay_state();
        }

        // A death partway through this loop leaves some waiters unacked; each gets
        // `SubmitError::ReceiptLost` → 500, never `ExecutorDead` → 503, since its change is
        // durably in force.
        for entry in entries {
            if let Some(reply) = &entry.reply {
                reply.ack(());
            }
        }
    }

    /// A deny window's sync failed. Re-write its records and sync again, up to
    /// [`DENY_DURABILITY_ATTEMPTS`] times in total, and report whether durability was reached.
    ///
    /// The deny lane retries and the ingest lane does not: a deny window's failure applies its
    /// deletions and suppressions anyway, so a retry here can still change what the live node shows
    /// before a restart un-hides them. A bare second `fsync` is not a retry on Linux, since the
    /// kernel may report a writeback error exactly once; `Wal::retry_durability` rewinds to the
    /// last durable offset and re-writes the records instead. Runs before the window is applied,
    /// since entries order must stay apply order for a `suppress D` followed by an `unsuppress D`.
    pub(super) fn retry_deny_durability(
        &mut self,
        entries: &[DenyEntry],
        first: WalError,
    ) -> std::result::Result<(), WalError> {
        let records: Vec<WalRecord> = entries.iter().map(|e| e.record.clone()).collect();
        let mut last = first;
        for delay in DENY_DURABILITY_BACKOFF {
            std::thread::sleep(delay);
            match self.wal.retry_durability(&records) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Drains the work queue into one commit window and closes it. Returns whether anything was
    /// done, so [`Executor::run`] re-drains the deny lane instead of blocking.
    ///
    /// A window closes when it reaches `commit_window_max_rows`, when the queue is empty, or when an
    /// entry names an external id the window already holds. The row bound is checked inside the
    /// drain, because under load the queue never empties. Every close returns to the run loop, which
    /// is what bounds a deny's wait to the window in front of it: do not keep draining after a close.
    pub(super) fn run_work_pass(&mut self) -> bool {
        let max_rows = self.health.commit_window_max_rows();
        let mut window: CommitWindow<Reply<Ingested>> = CommitWindow::new(self.next_window_seq());
        let mut did_work = false;

        loop {
            if window.rows() >= max_rows {
                self.close_window(window);
                return true;
            }
            let Ok(work) = self.queues.work.try_recv() else {
                break;
            };
            let command = match work {
                ExecutorWork::Lifecycle(command) => command,
                ExecutorWork::PublishGeometry {
                    publication,
                    respond,
                } => {
                    // A publication swaps the whole generation, so the open window closes first.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    let _ = respond.send(self.publish_geometry(publication));
                    self.health.note_work_refused();
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::ForgetSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.forget_suggestion_index(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::RebuildSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
                    // The rebuild reads the live minter, which a window holding a minting ingest
                    // has not published yet.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.rebuild_suggestion_index_now(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
            };
            let Command::Ingest {
                rows,
                batch_id,
                body_hash,
                artifacts,
                reply,
            } = command
            else {
                // Applies immediately while a window holding earlier ingest is still open, so WAL
                // append order stops equalling submission order: tolerable here since none of
                // these touch the buffer or swap the generation.
                self.execute(command);
                self.health.note_work_refused();
                did_work = true;
                continue;
            };

            let admitted;
            let m = StageMark::now();
            (window, admitted) =
                self.admit_ingest(window, rows, batch_id, body_hash, artifacts, reply);
            self.health.lap(WriteStage::AdmitWindow, m);
            did_work = true;
            if admitted == Admission::YieldedAfterClose {
                break;
            }
        }

        if !window.is_empty() {
            self.close_window(window);
            did_work = true;
        }
        did_work
    }

    /// Close `window` and return its replacement.
    ///
    /// `CommitWindow::new` must stamp the replacement's `opened_at` after `close_window` runs, not
    /// before: stamped first, a replacement would charge its predecessor's whole service to itself,
    /// doubling `record_window_service` and the `retry_after_s` a shed client is told.
    pub(super) fn close_and_reopen(&mut self, window: CommitWindow<Reply<Ingested>>) -> CommitWindow<Reply<Ingested>> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    /// The prefix directory to write into, derived from the generation the caller is publishing
    /// against rather than remembered.
    ///
    /// A fold flips `CURRENT`, changing which prefix is live. A stored `PathBuf` rotated at the
    /// flip would have to be got right at every site that uses it; a derived value cannot be missed.
    pub(super) fn prefix_dir(&self, generation: &Generation) -> PathBuf {
        self.bundle_root.join(&generation.prefix)
    }

    /// Take the next side-manifest number: this executor's counter, raised over every
    /// `SEGMENTS-<n>.json` present under the bundle root.
    ///
    /// One publication, one scan: a caller allocating several numbers at once raises the floor
    /// itself and then takes each number from [`Executor::take_manifest_n`].
    pub(super) fn allocate_manifest_n(&mut self) -> tessera_store::Result<u64> {
        self.raise_manifest_floor()?;
        Ok(self.take_manifest_n())
    }

    /// Raise the counter over every `SEGMENTS-<n>.json` on disc, and alarm if it moved.
    ///
    /// A floor above the counter is positive evidence of a second writer: in single-writer
    /// operation the two are equal at every allocation. Raising the floor keeps this node
    /// publishing rather than colliding at every number it re-plans at.
    ///
    /// A bundle root that cannot be listed fails the allocation and so the publication: the caller
    /// discards, its files are orphans, and the next tick re-plans.
    pub(super) fn raise_manifest_floor(&mut self) -> tessera_store::Result<()> {
        let on_disk = tessera_store::highest_side_manifest_n(&self.bundle_root)?;
        let floor = on_disk.map_or(0, |highest| highest + 1);
        if floor > self.next_manifest_n {
            self.health
                .foreign_side_manifests
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                floor,
                counter = self.next_manifest_n,
                root = %self.bundle_root.display(),
                "ALARM: a side-manifest this executor did not write is on disc. One executor owns \
                 a bundle root; publications continue above it, and what the other writer has \
                 published is not reconciled with what this node holds"
            );
            self.next_manifest_n = floor;
        }
        Ok(())
    }

    /// The counter alone, for a caller that has just raised the floor.
    pub(super) fn take_manifest_n(&mut self) -> u64 {
        let n = self.next_manifest_n;
        self.next_manifest_n += 1;
        n
    }

    /// Commits one partition's side-manifest. Every publication writes its manifest through here,
    /// so two things are done once: the manifest's ordered scalars are checked against
    /// `live_manifest` ([`crate::geometry::check_manifest_publishable`]), since a manifest is
    /// assembled by editing a clone that may be stale; and the level versions and derived files are
    /// stamped ([`artifact_coordinates`]). A refusal writes nothing.
    pub(super) fn commit_side_manifest(
        &self,
        live_manifest: &tessera_store::manifest::SegmentsManifest,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        next: &mut tessera_store::manifest::SegmentsManifest,
        fold: Option<FoldDerived<'_>>,
    ) -> Result<(), ManifestCommitRefused> {
        // Only the fold brings its own derived files and levels pending retirement; every other
        // publication carries the held list forward.
        let (derived, pending_retirement) = match &fold {
            Some(fold) => (fold.written, fold.pending_retirement),
            None => (self.derived_extents.as_slice(), &[][..]),
        };
        let (level_versions, derived_extents) = self
            .live
            .with_artifacts(|store| artifact_coordinates(store, derived, pending_retirement));
        next.level_versions = level_versions;
        next.derived_extents = derived_extents;
        crate::geometry::check_manifest_publishable(live_manifest, next)
            .map_err(ManifestCommitRefused::Regresses)?;
        tessera_store::write_segments_manifest(prefix_dir, partition, n, next)
            .map_err(ManifestCommitRefused::Store)
    }

    pub(super) fn next_window_seq(&mut self) -> u64 {
        self.window_seq += 1;
        self.window_seq
    }

    /// The batch-id state machine, evaluated on the executor.
    ///
    /// Takes the open window by value and hands it back, possibly replaced, so the conflict path
    /// cannot construct the replacement before the close it replaces.
    ///
    /// Lookup order is durable index, then open window, then unknown, evaluated here rather than in
    /// the handler: between a handler check and the enqueue the window can close, so a retry that
    /// saw unknown and then enqueued into a fresh window would have double-allocated.
    pub(super) fn admit_ingest(
        &mut self,
        mut window: CommitWindow<Reply<Ingested>>,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        reply: Reply<Ingested>,
    ) -> (CommitWindow<Reply<Ingested>>, Admission) {
        match BatchState::of(&self.live, &window, &batch_id) {
            BatchState::Accepted {
                body_hash: prev_hash,
                entity_ids,
            } => {
                if prev_hash == body_hash {
                    // A replay mints nothing: this batch's keys were created when first accepted.
                    reply.ack(Ingested {
                            entity_ids,
                            minted: 0,
                        },
                    );
                } else {
                    reply.fail(ExecError::BatchConflict { batch_id });
                }
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Held {
                window_seq,
                body_hash: prev_hash,
            } => {
                debug_assert_eq!(
                    window_seq,
                    window.seq(),
                    "the entry must be joined to the window it was found in"
                );
                if prev_hash == body_hash {
                    let joined = window.join(&batch_id, reply);
                    debug_assert!(joined, "`held` just answered for this batch id");
                } else {
                    // The 409 reaches the retry, not the held original, which is still owed its ack.
                    reply.fail(ExecError::BatchConflict { batch_id });
                }
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Unknown => {
                let mut admission = Admission::Admitted;
                // Only external ids reach here: a held batch id was answered above.
                if window.holds_external_id_of(&rows) {
                    window = self.close_and_reopen(window);
                    admission = Admission::YieldedAfterClose;
                }
                if let Some(entry) = self.admit(rows, batch_id, body_hash, artifacts, reply) {
                    if window.is_empty() {
                        // Armed at the first entry: an empty window is never closed.
                        self.health.mark_work_started(window.opened_at());
                    }
                    window.push(entry);
                }
                (window, admission)
            }
        }
    }

    /// The external-id admission check, on the one thread that also performs the inserts. `None`
    /// means the caller has already been answered. This check reads state written at apply, which
    /// is why an entry naming an external id the open window holds must close it first.
    ///
    /// The membership column's keys resolve here too, and a bad one refuses the whole batch: one
    /// caller's typo must not refuse another caller's rows in the same window. On an open layer the
    /// same key mints, but its ordinal is not claimed here; see `Executor::mint_records`.
    pub(super) fn admit(
        &mut self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        reply: Reply<Ingested>,
    ) -> Option<WindowEntry<Reply<Ingested>>> {
        // The fail-closed backstop for the widened check-to-apply race, read from the same
        // generation the apply below clones from, so the deleted-holder exemption cannot race
        // its own delete.
        let generation = self.generation.load();
        let mut rows = rows;
        let collisions = self.live.established_collisions(
            &mut rows,
            |e| generation.overlay.is_deleted(e),
            |entity, view| {
                generation.bundle.partitions.values().any(|partition| {
                    partition
                        .views
                        .get(view)
                        .is_some_and(|data| data.row_space.row_of(entity).is_some())
                }) || generation.buffer.contains_in_view(entity, view)
            },
        );
        if collisions == 0 {
            if let Err(detail) = settle_joins(&generation, &mut rows) {
                drop(generation);
                reply.fail(ExecError::JoinRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        }
        drop(generation);
        if collisions > 0 {
            reply.fail(ExecError::DuplicateExternalId { count: collisions },
            );
            self.health.note_work_refused();
            return None;
        }

        let (memberships, edges) = match self.resolve_memberships(&artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                reply.fail(ExecError::LayerRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        };

        Some(WindowEntry {
            rows,
            batch_id,
            body_hash,
            memberships,
            edges,
            waiters: vec![reply],
        })
    }

    /// Resolve one batch's membership keys, and check the edges its adjacency declared.
    ///
    /// Returns the memberships, each carrying the ordinal it resolved to or `None` where an open
    /// layer will mint it at the close, and the edges whose child is one of those mints, which are
    /// the only edges this route creates rather than checks. `Err` is the refusal text the caller
    /// is answered with, whole batch without effect.
    ///
    /// The memberships resolve first: neither a child nor a parent can be minted without appearing
    /// in the resolved set the edge checks read.
    pub(super) fn resolve_memberships(
        &self,
        artifacts: &tessera_lifecycle::BatchArtifacts,
    ) -> Result<
        (
            Vec<tessera_lifecycle::ResolvedMembership>,
            Vec<tessera_lifecycle::BatchEdge>,
        ),
        String,
    > {
        if artifacts.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        // One line per batch, not one per edge.
        let mut unrecorded: Vec<String> = Vec::new();
        let mut unrecorded_total = 0usize;
        let resolved = self.live.with_publication_state(|registry, store, _| {
            let memberships: Vec<tessera_lifecycle::ResolvedMembership> = artifacts
                .memberships
                .iter()
                .map(|join| {
                    registry
                        .resolve_or_mint(&join.layer, join.level, &join.key, store)
                        .map(|ordinal| tessera_lifecycle::ResolvedMembership {
                            layer: join.layer.clone(),
                            level: join.level,
                            key: join.key.clone(),
                            ordinal,
                            rows: join.rows.clone(),
                        })
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<_, String>>()?;
            // Two indexes of one set: a child's level is the edge's own, asked precisely; a
            // parent's is asked of the layer. A levelled taxonomy legitimately carries one key at
            // two levels, and one index would treat minting at one as minting at both.
            let minting: std::collections::BTreeSet<(&str, u32, &str)> = memberships
                .iter()
                .filter(|m| m.ordinal.is_none())
                .map(|m| (m.layer.as_str(), m.level, m.key.as_str()))
                .collect();
            let anywhere: std::collections::BTreeSet<(&str, &str)> = minting
                .iter()
                .map(|(layer, _, key)| (*layer, *key))
                .collect();

            // Only a `nested` or `tiered` list column declares edges; a `dag` layer's several
            // parents arrive on its artifact rows' `parent` list by the publish route, never here.
            parent_of_each_child(&artifacts.edges)?;

            let mut mints = Vec::new();
            for edge in &artifacts.edges {
                let layer = edge.layer.as_str();
                match registry.check_edge(
                    edge,
                    store,
                    minting.contains(&(layer, edge.level, edge.child.as_str())),
                    &|key| anywhere.contains(&(layer, key)),
                ) {
                    Ok(tessera_lifecycle::EdgeCheck::Agrees) => {}
                    // The child does not exist yet, so this edge is carried to the close, where
                    // the artifact is created and lineage is settled.
                    Ok(tessera_lifecycle::EdgeCheck::Mints) => mints.push(edge.clone()),
                    // Reported, not refused: the membership half of the same entry lands; only the
                    // edge this route cannot create is lost.
                    Ok(tessera_lifecycle::EdgeCheck::Unrecorded) => {
                        unrecorded_total += 1;
                        if unrecorded.len() < 5 {
                            unrecorded.push(format!(
                                "{} of {} under {}",
                                edge.child, edge.layer, edge.parent
                            ));
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok((memberships, mints))
        });
        if unrecorded_total > 0 {
            tracing::warn!(
                count = unrecorded_total,
                examples = ?unrecorded,
                "an ingest batch's list column names parent edges these layers do not hold; the \
                 memberships are applied and the edges are not. A growth adds members, and \
                 lineage is declared where the artifact is published"
            );
        }
        resolved
    }

    /// The artifacts this window's values named and nothing holds: one per
    /// `membership = { attribute = f }` layer whose column carried a value the level has no
    /// artifact for. Uses the same key rule as a build's mint
    /// (`tessera_types::layer::attribute_value_key`). Runs after the vocabulary mint: a novel
    /// category key is a string in the row until that pass draws it a code.
    ///
    /// A suppressed value's key still resolves and mints nothing, since
    /// [`ArtifactStore::ordinal_of_key`] loses a key only when the fold retires the artifact's own
    /// entity; a deleted one does mint again, since the new artifact is a new entity.
    pub(super) fn derive_records(
        &mut self,
        closed: &[tessera_lifecycle::ClosedEntry<Reply<Ingested>>],
        vocabularies: &Vocabularies,
    ) -> Result<Vec<WalRecord>, String> {
        use tessera_types::layer::attribute_value_key;

        // Which declared scalar each predicate layer reads, resolved once.
        let generation = self.generation.load();
        let declared = &generation.bundle.manifest.declared_scalars;
        let predicates: Vec<(String, usize, Option<String>)> =
            self.live.predicate_columns(|field| {
                let index = declared.iter().position(|scalar| scalar.name == field)?;
                Some((index, declared[index].vocabulary.clone()))
            });
        if predicates.is_empty() {
            return Ok(Vec::new());
        }

        let mut records = Vec::new();
        for (layer, index, vocabulary) in predicates {
            // `code → key`, walked from the live bindings rather than inverted per row.
            let mut key_of_code: std::collections::BTreeMap<u32, String> = Default::default();
            if let Some(name) = &vocabulary {
                if let Some(minter) = vocabularies.get(name) {
                    for (key, code) in minter.bindings() {
                        key_of_code.insert(code, key.to_string());
                    }
                }
            }
            let mut wanted: std::collections::BTreeSet<String> = Default::default();
            for entry in closed {
                for row in entry.rows() {
                    let Some(code) = row.scalars.get(index).and_then(scalar_code) else {
                        continue;
                    };
                    // Code 0 is a category code space's reserved *absent* sentinel and names no
                    // value; a plain integer column has no such reservation.
                    if vocabulary.is_some() && code == tessera_store::vocabulary::ABSENT_CODE {
                        continue;
                    }
                    wanted.insert(attribute_value_key(
                        code,
                        key_of_code.get(&code).map(String::as_str),
                    ));
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let prepared = self.live.with_publication_state(|registry, store, alloc| {
                let fresh: Vec<String> = wanted
                    .iter()
                    // A predicate layer is entity-scoped, so the key sits in the one set.
                    .filter(|key| store.ordinal_of_key(&layer, 0, None, key).is_none())
                    .cloned()
                    .collect();
                if fresh.is_empty() {
                    return Ok(None);
                }
                registry
                    .prepare_derive(&layer, 0, &fresh, store, alloc)
                    .map(Some)
                    .map_err(|e| e.to_string())
            })?;
            if let Some(record) = prepared {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Prepare the publications that create every artifact this window's rows named and nothing
    /// holds. See [`mint_plan`] for what is minted and why it is minted here.
    ///
    /// Patches the memberships whose key resolved since admission to the ordinal it resolved to, so
    /// they grow rather than mint; leaves a minted key's ordinal `None`, which is what tells
    /// [`growth_records`] the publication carried the join. `Err` is the refusal text every waiter
    /// is answered with: everything a single batch can be refused for alone was refused at admission.
    pub(super) fn mint_records(
        &mut self,
        closed: &mut [tessera_lifecycle::ClosedEntry<Reply<Ingested>>],
    ) -> Result<(Vec<WalRecord>, Vec<u64>), String> {
        let mut minted_per_entry = vec![0u64; closed.len()];
        let Some((wanted, edges)) = mint_plan(closed) else {
            return Ok((Vec::new(), minted_per_entry));
        };
        let (records, resolved, minted) = self.prepare_mints(&wanted, &edges)?;

        // A key that acquired an artifact between its batch's admission and this close is an
        // ordinary growth. See [`settle_resolved_ordinals`].
        for entry in closed.iter_mut() {
            settle_resolved_ordinals(&mut entry.memberships, &resolved);
        }
        for ((layer, level, key), (index, _)) in &wanted {
            if minted.contains(&(layer.clone(), *level, key.clone())) {
                minted_per_entry[*index] += 1;
            }
        }
        Ok((records, minted_per_entry))
    }

    /// Prepare one set of mints: the publications that create the artifacts a caller's keys named
    /// and no artifact holds.
    ///
    /// One implementation across the doors: `/control/ingest` reaches it through
    /// [`Executor::mint_records`] and `POST /control/values` through `values_mint_plan`, so a key
    /// arriving at either door creates the same artifact, with the same lineage and refusals.
    ///
    /// The three answers: the records to append in order, ascending level, coarse first; the keys
    /// that turned out to be held after all, which their caller grows into instead; and the keys
    /// this run minted.
    pub(super) fn prepare_mints(
        &self,
        wanted: &MintPlan,
        edges: &[tessera_lifecycle::BatchEdge],
    ) -> PreparedMints {
        use std::collections::BTreeMap;
        self.live.with_publication_state(|registry, store, alloc| {
            // Checked before anything is prepared, so a refusal spends nothing.
            let parents = parent_of_each_child(edges)?;
            // Re-resolved here, not trusted from admission: a publication may execute between an
            // admission and this close.
            let mut resolved: BTreeMap<(String, u32, String), u32> = BTreeMap::new();
            let mut to_mint: BTreeMap<(&str, u32), Vec<(&str, &croaring::Bitmap)>> =
                BTreeMap::new();
            for ((layer, level, key), (_, members)) in wanted {
                // The ingest route carries no artifact view, so the key sits in the one set.
                match store.ordinal_of_key(layer, *level, None, key) {
                    Some(ordinal) => {
                        resolved.insert((layer.clone(), *level, key.clone()), ordinal);
                    }
                    None => to_mint
                        .entry((layer.as_str(), *level))
                        .or_default()
                        .push((key.as_str(), members)),
                }
            }

            // Ascending level, one record each, coarse first: a tiered chain's parent sits one
            // level up and is fixed by the record before this one.
            let mut assigned: BTreeMap<(&str, u32, &str), u32> = BTreeMap::new();
            let mut records = Vec::new();
            for ((layer, level), keys) in &to_mint {
                let incoming: Vec<tessera_lifecycle::IncomingArtifact> = keys
                    .iter()
                    .map(|(key, members)| tessera_lifecycle::IncomingArtifact {
                        key: Some((*key).to_string()),
                        view: None,
                        members: (*members).clone(),
                        excluding: None,
                        contents: Vec::new(),
                        attached_to: None,
                        parent_keys: parents
                            .get(&(*layer, *level, *key))
                            .map(|parent| vec![(*parent).to_string()])
                            .unwrap_or_default(),
                        shape: None,
                    })
                    .collect();
                // One level up and no further: entry k of a list is the parent of entry k+1.
                let pending = |key: &str| {
                    let coarser = level.checked_sub(1)?;
                    assigned.get(&(*layer, coarser, key)).map(|ordinal| {
                        tessera_lifecycle::wal::ParentRef {
                            level: coarser,
                            ordinal: *ordinal,
                        }
                    })
                };
                let record = registry
                    .prepare_publish(layer, *level, &incoming, store, alloc, &pending)
                    .map_err(|e| e.to_string())?;
                let WalRecord::ArtifactPublish { artifacts, .. } = &record else {
                    unreachable!("prepare_publish returns an ArtifactPublish");
                };
                // Read back off the record, not recomputed: it is what this record actually claimed.
                for ((key, _), artifact) in keys.iter().zip(artifacts) {
                    debug_assert_eq!(artifact.key.as_deref(), Some(*key));
                    assigned.insert((*layer, *level, key), artifact.ordinal);
                }
                records.push(record);
            }
            let minted = assigned
                .keys()
                .map(|(layer, level, key)| ((*layer).to_string(), *level, (*key).to_string()))
                .collect();
            Ok((records, resolved, minted))
        })
    }

    /// Close a commit window: one signature-sorted allocation run, one WAL record per entry, one
    /// fsync, one generation swap, then every waiter is acked. Allocation is unchanged from a
    /// single batch's, except that the sort scope is the window.
    ///
    /// A failed window burns entity ids, exactly as a failed batch did: assignment precedes the
    /// append, so ids given to a window whose append then fails are never issued again.
    ///
    /// An append or fsync failure applies nothing, in deliberate contrast to the deny path: the
    /// apply-anyway rule is written for `Delete`/`Suppress` only. A restart does not undo the
    /// refusal: replay discards every record past the last fsync.
    pub(super) fn close_window(&mut self, window: CommitWindow<Reply<Ingested>>) {
        let entries = window.len() as u64;
        let started = window.opened_at();
        let mut mark = StageMark::now();

        let closed = match self.live.with_allocator(|a| window.allocate(a)) {
            Ok((closed, tally)) => {
                // Recorded at the allocation rather than after the append: a window that allocates
                // and then fails its append has still fragmented the entity axis this much.
                self.health.record_fragmentation(tally);
                closed
            }
            Err((e, waiters)) => {
                self.fail_window_alloc(e, waiters, entries, started);
                return;
            }
        };

        mark = self.health.lap(WriteStage::Allocate, mark);

        // Mint every novel discovered-vocabulary key this window's rows carry, in place, before
        // anything is appended, against a mutable copy of the published bindings that becomes the
        // next generation's if the window survives. One copy for the whole window, not one per
        // row: a second row naming an already-minted-this-window key sees the first row's binding.
        let generation = self.generation.load_full();
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        let declared_scalars = generation.bundle.manifest.declared_scalars.clone();
        // A row admitted under an earlier generation is padded here: a column declared since
        // admission appended at the tail of `declared_scalars`. Padded before the mint pass below
        // indexes by declared position, and before the append.
        let mut closed = closed;
        for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                crate::attributes::pad_to_schema(&mut row.scalars, &declared_scalars);
            }
        }
        // The group-scoped families, by the view a row names, derived once for the window.
        let scoped_by_view: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
            scoped_families_by_view(&generation.bundle.manifest);
        let mut fresh_bindings: Vec<(String, String, u32)> = Vec::new();
        let mut mint_failed: Option<MintError> = None;
        'minting: for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                for (index, declared) in declared_scalars.iter().enumerate() {
                    let Some(vocabulary) = declared.vocabulary.as_deref() else {
                        continue;
                    };
                    let WalScalar::Utf8(key) = &row.scalars[index] else {
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "column '{}' names vocabulary '{vocabulary}', which the live bindings \
                             do not carry",
                            declared.name
                        )
                    });
                    match minter.mint(&key) {
                        Ok(Minted::Fresh(code)) => {
                            fresh_bindings.push((vocabulary.to_string(), key, code));
                            row.scalars[index] = code_at_declared_width(declared.arrow_type, code);
                        }
                        Ok(Minted::Existing(code)) => {
                            row.scalars[index] = code_at_declared_width(declared.arrow_type, code);
                        }
                        Err(e) => {
                            mint_failed = Some(e);
                            break 'minting;
                        }
                    }
                }
                // The same mint, over the row's scoped tail.
                let Some(families) = scoped_by_view.get(row.view.as_str()) else {
                    continue;
                };
                for (index, family) in families.iter().enumerate() {
                    let Some(vocabulary) = family.vocabulary.as_deref() else {
                        continue;
                    };
                    let Some(WalScalar::Utf8(key)) = row.scoped.get(index) else {
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "scoped column family '{}' names vocabulary '{vocabulary}', which \\
                             the live bindings do not carry",
                            family.name
                        )
                    });
                    match minter.mint(&key) {
                        Ok(Minted::Fresh(code)) => {
                            fresh_bindings.push((vocabulary.to_string(), key, code));
                            row.scoped[index] = code_at_declared_width(family.arrow_type, code);
                        }
                        Ok(Minted::Existing(code)) => {
                            row.scoped[index] = code_at_declared_width(family.arrow_type, code);
                        }
                        Err(e) => {
                            mint_failed = Some(e);
                            break 'minting;
                        }
                    }
                }
            }
        }
        if let Some(e) = mint_failed {
            // Nothing has been appended yet, so the window has no effect.
            let detail = e.to_string();
            self.fail_window(
                closed,
                || ExecError::VocabularyRefused {
                    detail: detail.clone(),
                },
                entries,
                started,
            );
            return;
        }

        // Prepared before anything is appended, so a refusal spends nothing.
        let (mut mint_records, minted_per_entry) = match self.mint_records(&mut closed) {
            Ok(minted) => minted,
            Err(detail) => {
                self.fail_window(
                    closed,
                    || ExecError::LayerRefused {
                        detail: detail.clone(),
                    },
                    entries,
                    started,
                );
                return;
            }
        };
        // Runs after the vocabulary mint above, since a novel category key is a code only once
        // that pass has drawn it, and the key an artifact is named by is the value's key.
        match self.derive_records(&closed, &vocabularies) {
            Ok(records) => mint_records.extend(records),
            Err(detail) => {
                self.fail_window(
                    closed,
                    || ExecError::LayerRefused {
                        detail: detail.clone(),
                    },
                    entries,
                    started,
                );
                return;
            }
        }

        // One record per entry, appended in entries order, which is also apply order.
        let mut failed_at: Option<(usize, WalError)> = None;

        // Mint records land first, ahead of every batch record, inside the one fsync below, so a
        // mint is durable in the same commit as the rows it colours.
        for (vocabulary, key, code) in &fresh_bindings {
            if let Err(e) = self.wal.append(&WalRecord::VocabularyMint {
                vocabulary: vocabulary.clone(),
                key: key.clone(),
                code: *code,
            }) {
                failed_at = Some((0, e));
                break;
            }
        }

        // The position before each append is the only moment it can be read: afterwards the log
        // has moved on. A rotation reclaims by it below, so a failed append contributes none.
        let mut positions: Vec<u64> = Vec::with_capacity(closed.len());
        if failed_at.is_none() {
            for (i, entry) in closed.iter().enumerate() {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&entry.record) {
                    failed_at = Some((i, e));
                    break;
                }
                positions.push(at);
            }
        }

        // The joins this window's rows declared, appended behind the batch records. The
        // publications that minted come first, since an artifact must exist before anything
        // addresses it.
        let mut minted: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for record in mint_records {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    failed_at = Some((0, e));
                    break;
                }
                minted.push((record, at));
            }
        }
        let mut growth: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for (record, i) in growth_records(&closed) {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    failed_at = Some((i, e));
                    break;
                }
                growth.push((record, at));
            }
        }
        mark = self.health.lap(WriteStage::WalAppend, mark);
        // One fsync for the whole window: the amortisation half of group commit.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                // The first waiter gets the real error arbitrarily and the rest `Poisoned`.
                failed_at = Some((0, e));
            }
        }
        self.health.lap(WriteStage::WalFsync, mark);
        self.observe_wal();
        if let Some((index, error)) = failed_at {
            self.fail_window_wal(closed, index, error, entries, started);
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);
        self.apply_window(&mut closed, &positions, vocabularies, &fresh_bindings);

        // After the rows are in force, never before: a membership is projected through rows.
        let (artifact_records, artifact_positions): (Vec<&WalRecord>, Vec<u64>) = minted
            .iter()
            .chain(growth.iter())
            .map(|(record, position)| (record, *position))
            .unzip();
        self.apply_artifact_records(&artifact_records, &artifact_positions);

        // Recorded after the swap, so a replay can never see it swapped but not yet indexed.
        let m = StageMark::now();
        for (entry, wal_pos) in closed.iter().zip(&positions) {
            let (batch_id, body_hash) = entry.batch_key();
            debug_assert_eq!(
                tessera_lifecycle::batch_identity(&entry.record),
                Some(tessera_lifecycle::BatchIdentity {
                    batch_id,
                    body_hash,
                    allocation: entry.entity_ids.clone(),
                }),
                "the accepted-batch index disagrees with the record it caches"
            );
            self.live.record_accepted_batch(
                batch_id.to_string(),
                body_hash,
                entry.entity_ids.clone(),
                *wal_pos,
            );
        }
        self.health.lap(WriteStage::RecordBatch, m);
        self.observe_wal();

        // Under `value_set = "open"` a typo creates a permanent object rather than being refused,
        // so the caller is told the count in its own 200 and the operator gets this line.
        let created: u64 = minted_per_entry.iter().sum();
        if created > 0 {
            tracing::info!(
                minted = created,
                artifacts = ?minted
                    .iter()
                    .flat_map(|(record, _)| match record {
                        WalRecord::ArtifactPublish { layer, level, artifacts, .. } => artifacts
                            .iter()
                            .filter_map(|a| a.key.as_ref())
                            .map(|key| format!("{key} in level {level} of {layer}"))
                            .take(8)
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                "an ingest batch named keys no artifact held, and this layer's value set is open, \
                 so they were created carrying nothing but their names"
            );
        }

        // A death partway through this loop leaves some waiters unacked; each gets
        // `SubmitError::ReceiptLost` → 500, never `ExecutorDead` → 503, since its ingest is
        // durably in force.
        for (entry, minted) in closed.into_iter().zip(minted_per_entry) {
            let ClosedEntry {
                entity_ids,
                mut waiters,
                ..
            } = entry;
            let last = waiters
                .pop()
                .expect("an entry always has at least one waiter");
            for waiter in waiters {
                let entity_ids = entity_ids.clone();
                waiter.ack(Ingested { entity_ids, minted });
            }
            last.ack(Ingested { entity_ids, minted });
        }

        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window could not be allocated: nothing was appended, nothing applied, and the high-water
    /// mark did not move. `AllocError` is `Copy`, so every waiter gets the real one.
    pub(super) fn fail_window_alloc(
        &self,
        error: AllocError,
        waiters: Vec<Vec<Reply<Ingested>>>,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in waiters {
            for waiter in entry {
                waiter.fail(ExecError::Alloc(error));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window's append or fsync failed: apply nothing, and answer every waiter.
    pub(super) fn fail_window_wal(
        &self,
        closed: Vec<ClosedEntry<Reply<Ingested>>>,
        index: usize,
        error: WalError,
        entries: u64,
        started: std::time::Instant,
    ) {
        let mut real = Some(error);
        for (i, entry) in closed.into_iter().enumerate() {
            for (k, waiter) in entry.waiters.into_iter().enumerate() {
                // The real error goes to the entry the failure belongs to; every other waiter gets
                // `Poisoned`, which is precisely what its own append would have returned had it
                // been attempted after the failure, and what the WAL will in fact return for
                // every subsequent call.
                let e = if i == index && k == 0 {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                waiter.fail(ExecError::Wal(e));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// Answers every waiter of a window that was refused after allocation with the same error.
    pub(super) fn fail_window(
        &self,
        closed: Vec<ClosedEntry<Reply<Ingested>>>,
        error: impl Fn() -> ExecError,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in closed {
            for waiter in entry.waiters {
                waiter.fail(error());
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// Hold one accepted write's delta until the tick. Every route that changes a level's records
    /// arrives here with the level version it followed, and the level's row forms take the run of
    /// them at the next tick.
    ///
    /// `refused` names the growth entries the store did not take; they are held for nothing. A
    /// record whose every entry was refused is still held, empty, since the versions of an
    /// interval's deltas must stay consecutive.
    pub(super) fn hold_delta(&mut self, record: &WalRecord, before: u64, refused: &[usize]) {
        let (layer, level, kind) = match record {
            WalRecord::ArtifactPublish {
                layer,
                level,
                artifacts,
                ..
            } => (
                layer,
                *level,
                crate::artifacts::DeltaKind::Published(
                    artifacts.iter().map(|a| a.ordinal).collect(),
                ),
            ),
            WalRecord::ArtifactGrow {
                layer,
                level,
                growth,
            } => {
                let mut joins = Vec::new();
                let mut pages = Vec::new();
                for (index, grown) in growth.iter().enumerate() {
                    if refused.contains(&index) {
                        continue;
                    }
                    let Some(joining) =
                        tessera_lifecycle::membership::deserialise_members(&grown.joining)
                    else {
                        continue;
                    };
                    match grown.set {
                        tessera_lifecycle::wal::GrownSet::Membership => {
                            joins.push((grown.ordinal, joining))
                        }
                        tessera_lifecycle::wal::GrownSet::GeneratingSet { rank, cardinality } => {
                            // A leave, or a withdrawal, re-derives the operator whole: a union
                            // cannot express a leave.
                            let leaves =
                                tessera_lifecycle::membership::deserialise_leaving(&grown.leaving)
                                    .is_none_or(|leaving| !leaving.is_empty());
                            pages.push(crate::artifacts::SetPage {
                                ordinal: grown.ordinal,
                                rank,
                                joining,
                                whole: leaves || cardinality == 0,
                            });
                        }
                    }
                }
                (
                    layer,
                    *level,
                    crate::artifacts::DeltaKind::Grown { joins, pages },
                )
            }
            WalRecord::ArtifactFill {
                layer,
                level,
                ordinal,
                ..
            } => (
                layer,
                *level,
                crate::artifacts::DeltaKind::Filled(vec![*ordinal]),
            ),
            _ => return,
        };
        self.pending_forms
            .entry((layer.clone(), level))
            .or_default()
            .push(crate::artifacts::LevelDelta { before, kind });
    }

    /// Appends the records and fsyncs once, and returns the position each record took. The
    /// position is read before the append, because it is the bound rotation must not reclaim past.
    /// On failure nothing the command prepared is in force; ids it reserved stay spent, so a torn
    /// append that replays cannot land them on entities a later command also holds.
    pub(super) fn make_durable(&mut self, records: &[&WalRecord], what: &str) -> Result<Vec<u64>, ExecError> {
        let mut positions = Vec::with_capacity(records.len());
        let appended = records.iter().try_for_each(|record| {
            positions.push(self.wal.position());
            self.wal.append(record)
        });
        let durable = appended.and_then(|()| self.wal.fsync());
        self.observe_wal();
        match durable {
            Ok(_) => Ok(positions),
            Err(e) => {
                tracing::error!(error = %e, "ALARM: {what} could not be made durable; none of it is in force");
                Err(ExecError::Wal(e))
            }
        }
    }

    /// Applies durable artifact records to the registry and the store, and holds the delta each
    /// made for the tick that brings the level's row forms forward. Each record moves its level's
    /// version by one, so the version a delta starts from walks with the records.
    pub(super) fn apply_artifact_records(&mut self, records: &[&WalRecord], positions: &[u64]) {
        if records.is_empty() {
            return;
        }
        let befores: Vec<u64> = self.live.with_artifacts(|store| {
            let mut seen: std::collections::BTreeMap<(&str, u32), u64> = Default::default();
            records
                .iter()
                .map(|record| {
                    let Some((layer, level)) = artifact_level_of(record) else {
                        return 0;
                    };
                    let at = seen
                        .entry((layer, level))
                        .or_insert_with(|| store.level_version(layer, level));
                    let version = *at;
                    *at += 1;
                    version
                })
                .collect()
        });
        let mut refused_per_record: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
        let undecodable = self.live.with_publication_state(|registry, store, _| {
            records
                .iter()
                .zip(positions)
                .zip(refused_per_record.iter_mut())
                .map(|((record, position), refused)| {
                    registry.apply(record);
                    store.apply_reporting(record, *position, refused)
                })
                .sum::<usize>()
        });
        if undecodable > 0 {
            tracing::error!(
                count = undecodable,
                "ALARM: artifact records did not survive their own round trip; those artifacts are absent"
            );
        }
        for ((record, before), refused) in records.iter().zip(befores).zip(&refused_per_record) {
            self.hold_delta(record, before, refused);
        }
        // Durable in the log and not yet in a manifest.
        self.deny_dirty = true;
    }

    /// Clone the buffer once, insert every entry in the window, publish once. The clone is
    /// O(total buffered items), so a window of k entries pays it once instead of k times.
    ///
    /// `terms` is taken out of each entry rather than borrowed, since the ack needs `entity_ids`,
    /// not terms. `vocabularies` is `close_window`'s locally mutated copy, published verbatim
    /// rather than `Arc::clone(&generation.vocabularies)`: cloning the old `Arc` here would
    /// silently discard every code this window just drew.
    pub(super) fn apply_window(
        &self,
        closed: &mut [ClosedEntry<Reply<Ingested>>],
        positions: &[u64],
        vocabularies: Vocabularies,
        mints: &[(String, String, u32)],
    ) {
        let started = std::time::Instant::now();
        let mut mark = StageMark::now();
        let generation = self.generation.load_full();
        // The suggestion index's side map, grown by exactly the keys this window minted. Nothing
        // is rebuilt: every other vocabulary's index is carried behind its `Arc`.
        let suggest = generation.suggest.with_mints(
            &tessera_analyse::SuggestionFold::new(),
            &vocabularies,
            mints,
        );
        let mut buffer = (*generation.buffer).clone();
        mark = self.health.lap(WriteStage::ApplyBufferClone, mark);

        let mut established = lock_recover(&self.live.established);
        // Updated together in one critical section, so a `/control/changes` lookup and a
        // `/v1/items` drill-down can never disagree about the same item.
        let mut established_inverse = lock_recover(&self.live.established_inverse);
        for (entry, wal_pos) in closed.iter_mut().zip(positions) {
            let terms = std::mem::take(&mut entry.terms);
            for (row, row_terms) in entry.rows().iter().zip(terms) {
                // No external id means nothing to establish.
                let mut m = StageMark::now();
                if let Some(external_id) = &row.external_id {
                    established.insert(external_id.clone(), row.entity_id);
                    m = self.health.lap(WriteStage::RowEstablished, m);
                    established_inverse.insert(row.entity_id, external_id.clone());
                    m = self.health.lap(WriteStage::RowEstablishedInv, m);
                }
                buffer.insert_row_with_terms(row, row_terms);
                let m = self.health.lap(WriteStage::RowBufferInsert, m);
                buffer.set_wal_pos(row.entity_id, &row.view, *wal_pos);
                self.health.lap(WriteStage::RowWalPos, m);
            }
        }
        drop(established);
        drop(established_inverse);
        mark = self.health.lap(WriteStage::ApplyRows, mark);

        // Published here, and at every other place buffer occupancy changes, so
        // `/control/ingest`'s occupancy bound reads a figure the executor maintains.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);

        let next = generation.with(|g| {
            g.overlay_version = generation.overlay_version + 1;
            g.buffer = Arc::new(buffer);
            g.vocabularies = Arc::new(vocabularies);
            // The one publication that changes the suggestion index, by the same mints that
            // changed the bindings above. Every other publication carries the index forward.
            g.suggest = suggest;
        });
        self.publish(next, started);
        self.health.lap(WriteStage::ApplySwap, mark);
    }

    /// Clone the overlay once, apply every change in the window, publish once. [`Overlay`] never
    /// shrinks except at a fold, so the clone is O(overlay depth). Changes are applied in the
    /// window's entries order, the deny lane's FIFO order, so a `suppress` and a later `unsuppress`
    /// of the same item resolve as two separate commands would have.
    ///
    /// Pins are never invalidated by this: a pin fixes `(prefix, segments_version)`, and this bumps
    /// `overlay_version` instead, so a suppression applies to a pinned request the moment accepted.
    pub(super) fn apply_changes(&self, changes: Vec<(EntityId, ChangeOp)>) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        let mut newly_denied: Vec<EntityId> = Vec::new();
        let mut deleted: Vec<EntityId> = Vec::new();
        let mut unsuppressed = false;
        for (entity, op) in changes {
            match op {
                ChangeOp::Delete => {
                    newly_denied.push(entity);
                    deleted.push(entity);
                }
                ChangeOp::Suppress => newly_denied.push(entity),
                ChangeOp::Unsuppress => unsuppressed = true,
            }
            overlay.apply(entity, op);
        }

        // `overlay_soft_limit` gauges `deleted ∪ suppressed`, since a suppression never retires and
        // a fold dispatched on the union would rewrite the corpus to retire nothing.
        //
        // Edge-triggered: the depth never decreases, so a level-triggered check would emit this
        // WARN on every subsequent deny, forever, with no path back.
        let depth = overlay.len();
        let limit = self.health.overlay_soft_limit();
        if self.health.note_overlay_depth(depth) {
            tracing::warn!(
                overlay_depth = depth,
                overlay_soft_limit = limit,
                "ALARM: the overlay has crossed its configured soft limit. A compaction fold \
                 retires the executed deletions, but nothing schedules one automatically; watch \
                 overlay.depth on /control/status"
            );
        }

        // A deleted row leaves the buffer here: `plan_flush` never consumes a deleted row, so
        // nothing else would ever remove it, and a `delete` issued before the item's first flush
        // would otherwise pin the WAL forever.
        //
        // The clone is paid only when a buffered row is actually dropped. Deleting an entity that
        // already has geometry, the ordinary case, costs one hash lookup and no clone.
        let buffer = if !generation.buffer.holds_any(&deleted) {
            Arc::clone(&generation.buffer)
        } else {
            let mut buffer = (*generation.buffer).clone();
            for entity in deleted {
                buffer.remove(entity);
            }
            self.health
                .buffered_items
                .store(buffer.len(), Ordering::SeqCst);
            Arc::new(buffer)
        };

        // A window of deletes and suppressions only grows the mask, so their rows are added. An
        // unsuppress derives it afresh: subtracting a row would re-expose an entity that is still
        // deleted.
        let overlay_version = generation.overlay_version + 1;
        let next = if unsuppressed {
            generation.with(|g| {
                g.overlay_version = overlay_version;
                g.overlay = Arc::new(overlay);
                g.buffer = buffer;
            })
        } else {
            generation.with_denies(Arc::new(overlay), &newly_denied, |g| {
                g.overlay_version = overlay_version;
                g.buffer = buffer;
            })
        };
        self.publish(next, started)
    }

    /// Restate the live row-less state into a side-manifest about to be committed: the registry
    /// and its low-water mark, the roster, the attribute columns, the vocabularies, and the view
    /// groups and plain views.
    ///
    /// Restated from live state, never carried forward from the clone: a manifest a publication
    /// starts from may be several behind, so carrying a stale value forward would drop a
    /// registration. `min`, not `max`, for the mark: the row-less region grows downward.
    pub(super) fn write_live_state(&self, manifest: &mut SegmentsManifest, vocabularies: &Vocabularies) {
        let (layers, layer_tombstones, low_water) = self.live.registry_for_publication();
        manifest.entity_id_low_water = manifest.entity_id_low_water.min(low_water);
        manifest.layers = layers;
        manifest.layer_tombstones = layer_tombstones;
        let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
        manifest.views = created_views;
        manifest.dead_view_incarnations = dead_view_incarnations;
        let (attributes, scoped_attributes) = self.live.attributes_for_publication();
        manifest.attributes = attributes;
        manifest.scoped_attributes = scoped_attributes;
        manifest.vocabularies = self.live.vocabularies_for_publication(vocabularies);
        let (groups, plain_views) = self.live.view_declarations_for_publication();
        manifest.groups = groups;
        manifest.plain_views = plain_views;
    }

    /// Reclaim what the publication just made redundant: after the generation swap and never
    /// before it, since rotation writes its snapshot before any deletion. The reclaim bound is the
    /// buffer's oldest surviving row (`IngestBuffer::oldest_wal_pos`), refusing (`None`) rather
    /// than guessing if any buffered row does not know its own position.
    ///
    /// Two gates: a poisoned WAL cannot be appended to at all, and a node whose overlay has
    /// diverged from its durable WAL must rotate nothing, since writing a snapshot from that
    /// overlay would make a 500'd, never-acked deny permanent. Nothing here is fatal.
    pub(super) fn rotate_wal(&mut self) {
        if self.wal.is_poisoned() {
            return;
        }
        if !self.may_publish() {
            tracing::warn!(
                "this node's overlay has diverged from its durable WAL, so it rotates nothing; \
                 the log grows until an operator restarts it"
            );
            return;
        }

        let generation = self.generation.load();
        // A stepped-down node reclaims nothing: its WAL members are the only recovery material
        // for whatever the step-down shadowed.
        if generation
            .bundle
            .partitions
            .values()
            .any(|p| p.stepped_down())
        {
            tracing::warn!(
                "a partition is stepped down, so this node rotates nothing; the log grows until \
                 the damaged newest manifest is repaired"
            );
            return;
        }
        let reclaim_below = match generation.buffer.oldest_wal_pos() {
            // Nothing buffered: every ingest row has geometry, so the whole durable prefix is
            // reclaimable.
            None => self.wal.position(),
            Some(Some(oldest)) => oldest,
            // A buffered row of unknown position pins the log: fail-safe by construction, since the
            // sequence grows visibly rather than a record vanishing.
            Some(None) => 0,
        };
        // The oldest artifact publication pins the log too: a membership has no home outside the
        // WAL, so reclaiming a member holding one destroys the only copy. Fail-closed: a log that
        // grows is noticed where a membership that vanishes is not.
        let reclaim_below = match self.live.artifacts_oldest_wal_pos() {
            Some(oldest) => reclaim_below.min(oldest),
            None => reclaim_below,
        };

        let snapshot = generation.overlay.snapshot();
        match self.wal.rotate(&snapshot, reclaim_below) {
            Ok(deleted) => {
                // Post-rotation position, so the next growth check counts only appends made
                // after the snapshot this rotation just wrote.
                self.wal_position_at_last_rotation = self.wal.position();
                if !deleted.is_empty() {
                    // The idempotency index follows the log it caches, so entries whose records
                    // lay in the members just deleted go now: otherwise this process would answer
                    // a batch id as a replay that the same node would call unknown after a restart.
                    let forgotten = self.live.forget_batches_below(self.wal.retained_from());
                    tracing::info!(
                        members = ?self.wal.members(),
                        reclaimed = ?deleted,
                        forgotten_batch_ids = forgotten,
                        "WAL members reclaimed below the oldest unconsumed row"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the WAL did not rotate; the log grows until it does");
            }
        }
    }

    /// Rotate at the tick when the log has grown and no flush publication is coming to do it: the
    /// deny-only regime's rotation. Without this, a node that took denies without ever flushing
    /// would seal nothing, snapshot nothing and reclaim nothing: an unbounded log replayed in full
    /// at every restart.
    ///
    /// Gated on growth, so an idle node rotates nothing: a rotation writes an O(overlay) snapshot
    /// and a new member, which would be churn for no reclaim on a quiet deployment.
    pub(super) fn rotate_if_grown(&mut self) {
        if self.wal.position() == self.wal_position_at_last_rotation {
            return;
        }
        self.rotate_wal();
    }

    /// Read the WAL's size and its rotation bound into [`ExecutorHealth::wal_gauge`], at most once
    /// per tick period. Rotates nothing, compares nothing against a limit, returns no decision.
    ///
    /// The cost is O(members): under a `growth` or `fill` pin a member accumulates per rotation for
    /// as long as the fold that would release it is refused, so the walk gets dearer as the problem
    /// gets worse. The rate limit below bounds it to once per `flush_max_age_secs`.
    pub(super) fn sample_wal_gauge(&mut self) {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        if let Some(last) = self.last_wal_sample {
            if last.elapsed() < period {
                return;
            }
        }
        // Before the walk, so a slow walk shortens the next interval rather than pushing it out.
        self.last_wal_sample = Some(std::time::Instant::now());
        self.wal_samples += 1;
        let position = self.wal.position();
        let pin = self.live.with_artifacts(|store| store.wal_pin());
        self.health.record_wal_gauge(WalGauge {
            members: self.wal.member_count(),
            bytes: self.wal.disc_bytes(),
            position,
            pin,
            pin_span_bytes: pin.map_or(0, |(_, pos)| position.saturating_sub(pos)),
            samples: self.wal_samples,
        });
    }

    /// Publish new geometry: check, swap, prune. The executor's own arm of the swap-only
    /// publication step. On this thread there is nothing to race, so there is no
    /// compare-and-swap retry loop: one load, one check, one store.
    ///
    /// The one swap carries prefix, `segments_version`, watermark, bundle, dictionary and tier list
    /// always, and, when the publication carries a `PrefixRotation`, also the base postings, the
    /// fragment cache and identity it keys, the external-id sidecar, and the retirement of the
    /// executed deletions, all through the single `store` below: not a sequence a request could
    /// land between.
    pub(super) fn publish_geometry(
        &mut self,
        publication: GeometryPublication,
    ) -> std::result::Result<(), GeometryRefused> {
        let GeometryPublication {
            prefix,
            segments_version,
            watermark,
            bundle,
            dict,
            delta_postings,
            rotation,
        } = publication;
        let started = std::time::Instant::now();
        let previous = self.generation.load_full();
        check_publishable(&previous, &prefix, segments_version, watermark)?;

        // Rule F, in the fold's own swap and nowhere else: the overlay is cloned, retired against,
        // and published, never mutated in place on a shared `Arc` the read path is holding.
        // `overlay_version` moves only when something actually retired, since a geometry-only swap
        // that bumped it would falsely signal a change on the security-state axis cache keys read.
        let (overlay, overlay_version) = match rotation.as_ref().map(|r| &r.retired) {
            Some(retired) if !retired.is_empty() => {
                // The live external-id map loses the retired bindings first.
                let forgotten = self.live.forget_established(retired);
                let mut overlay = (*previous.overlay).clone();
                let count = overlay.retire(retired);
                tracing::info!(
                    retired = count,
                    forgotten_external_ids = forgotten,
                    prefix = %prefix,
                    "Rule F: executed deletions retired in the fold's own publication"
                );
                (Arc::new(overlay), previous.overlay_version + 1)
            }
            _ => (Arc::clone(&previous.overlay), previous.overlay_version),
        };


        let next = previous.with(|g| {
            g.prefix = prefix;
            // A rotation carries the new prefix's own columns; cloning the previous generation's
            // would serve the superseded prefix's mappings out of files reclamation is unlinking.
            g.filter_columns = rotation.as_ref().map_or_else(
                || Arc::clone(&previous.filter_columns),
                |r| Arc::clone(&r.filter_columns),
            );
            g.segments_version = segments_version;
            g.watermark = watermark;
            g.bundle = bundle;
            g.dict = dict;
            g.postings = rotation.as_ref().map_or_else(
                || Arc::clone(&previous.postings),
                |r| Arc::clone(&r.postings),
            );
            g.fragments = rotation.as_ref().map_or_else(
                || Arc::clone(&previous.fragments),
                |r| Arc::clone(&r.fragments),
            );
            g.external_index = rotation.as_ref().map_or_else(
                || Arc::clone(&previous.external_index),
                |r| Arc::clone(&r.external_index),
            );
            g.delta_postings = delta_postings;
            g.overlay_version = overlay_version;
            g.overlay = overlay;
            // The suggestion index carries across a rotation: a fold retires entities, never values.
        });
        // Listed before the swap, deleted after it: a listing taken before the swap can never name
        // an entry a request wrote after it, and nothing is deleted if the swap does not happen.
        let superseded = rotation
            .as_ref()
            .map(|_| previous.fragments.superseded_entries())
            .unwrap_or_default();

        let next = Arc::new(next);
        self.publish_arc(Arc::clone(&next), started);

        // A rotation refreshes after the swap and does not arm the shed: after a fold a missing
        // projection is an ordinary cache miss.
        if rotation.is_some() {
            self.refresh.spawn(next);
        }

        if !superseded.is_empty() {
            let swept = FragmentCache::sweep(&superseded);
            tracing::info!(
                swept,
                named = superseded.len(),
                "the fold's identity rotated; the persisted fragments under the superseded one are \
                 unreachable and have been reclaimed"
            );
        }

        // The retention pass, at the swap rather than at a reclaim.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        Ok(())
    }

    /// The generation swap. The only `store` in the write path.
    ///
    /// `load_full` + `store` is safe here because this is the sole thread that can publish: a
    /// flush must not `store` directly; it submits a command and is applied here.
    /// `scripts/check-layers.sh` refuses any non-atomic `.store(` in this crate's sources outside
    /// this file.
    pub(super) fn publish(&self, next: Generation, started: std::time::Instant) {
        self.publish_arc(Arc::new(next), started)
    }

    /// [`Self::publish`] over a generation the caller already holds by `Arc`: a geometry
    /// publication needs the same value afterwards, to hand the background refresh.
    pub(super) fn publish_arc(&self, next: Arc<Generation>, started: std::time::Instant) {
        self.generation.store(next);
        self.health
            .record_apply(started.elapsed().as_nanos() as u64);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Swap);
        }
    }

    /// Mirror the WAL's own poison flag into the posture, in both directions.
    ///
    /// Asked of the WAL rather than remembered from the last error this loop happened to see, so
    /// the posture cannot drift from the thing it describes.
    pub(super) fn observe_wal(&self) {
        self.health.mirror_wal(self.wal.is_poisoned());
    }

    /// Reach an armed pause site, if any. Fault-injection builds only; a no-op otherwise.
    ///
    /// Every call site holds no lock: a pause inside one would wedge this thread against its own
    /// waiters.
    #[cfg(feature = "fault-injection")]
    pub(super) fn pause_point(&self, site: PauseSiteArg) {
        use tessera_lifecycle::faults::PauseAction;
        let Some(faults) = &self.faults else { return };
        match faults.pause_point(site) {
            None | Some(PauseAction::Stall) => {}
            Some(PauseAction::Panic) => {
                panic!("fault-injection: executor panicked at the {site:?} pause point")
            }
        }
    }

    #[cfg(not(feature = "fault-injection"))]
    pub(super) fn pause_point(&self, _site: PauseSiteArg) {}

}
/// The pause-site argument, so the executor's call sites read the same in both builds.
///
/// In a fault-injection build this is [`tessera_lifecycle::faults::PauseSite`]. In a shipped
/// build the module does not exist, so it is a local zero-variant-cost stand-in and
/// `pause_point` is a no-op.
#[cfg(feature = "fault-injection")]
pub(super) type PauseSiteArg = tessera_lifecycle::faults::PauseSite;

#[cfg(not(feature = "fault-injection"))]
#[derive(Debug, Clone, Copy)]
pub(super) enum PauseSiteArg {
    AfterFsync,
    BeforeAck,
    BeforeManifestPublish,
    BeforeCurrentFlip,
    BeforeMergePublish,
}

/// The two rules the promotion design added to publication, tested where they are decided rather
/// than through a second view no build produces.
#[cfg(test)]
mod dispatch_rules_tests {
    use super::*;
    use tessera_lifecycle::BufferedItem;

    pub(super) fn plan_from(oldest: u64) -> crate::flush::FlushPlan {
        let item = BufferedItem {
            terms: Vec::new(),
            view: "s".to_string(),
            join: false,
            x: 0.5,
            y: 0.5,
            scalars: Vec::new(),
            scoped: Vec::new(),
            external_id: None,
            wal_pos: None,
        };
        crate::flush::FlushPlan {
            items: vec![(EntityId::new(oldest), item)],
            fills: Vec::new(),
            consumed_fills: Vec::new(),
            consumed_scoped_fills: Vec::new(),
        }
    }

    /// One plan is dispatched per tick, since a second unit in flight is discarded at its rebase.
    /// The one sent is the view whose oldest waiting row is oldest, not the first by name, which
    /// would let a continuously-fed `s0` deny `s1` a flush for ever.
    #[test]
    pub(super) fn a_dispatch_sends_the_plan_holding_the_oldest_unflushed_row() {
        let plans = vec![
            ("s0".to_string(), plan_from(900)),
            ("s1".to_string(), plan_from(100)),
            ("s2".to_string(), plan_from(500)),
        ];
        let (view, plan) = plan_to_dispatch(plans).expect("one of three");
        assert_eq!(view, "s1", "oldest row wins, not lowest view id");
        assert_eq!(plan.items[0].0.raw(), 100);
    }

    #[test]
    pub(super) fn a_dispatch_with_no_plans_sends_nothing() {
        assert!(plan_to_dispatch(Vec::new()).is_none());
    }

    /// The dictionary guard is scoped to flushes that wrote an extent. A flush that promoted
    /// nothing names only ordinals below the length it planned against, which append-only
    /// extension preserves, so discarding it would cost liveness and buy no safety.
    #[test]
    pub(super) fn only_a_promoting_flush_is_discarded_when_the_dictionary_moves() {
        // Promoted: its extent's ordinals are positions, and the positions have moved.
        assert!(dictionary_moved_under(Some(7), 9));
        assert!(!dictionary_moved_under(Some(7), 7));
        // Promoted nothing: never discarded, however far the dictionary has gone.
        assert!(!dictionary_moved_under(None, 9));
        assert!(!dictionary_moved_under(None, 0));
    }
}
