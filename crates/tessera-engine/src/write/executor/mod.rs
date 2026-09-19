use super::*;

mod commands;
mod fold;
mod publications;
mod values;

pub use commands::*;
pub(in crate::write) use fold::*;
pub(in crate::write) use publications::*;
use values::*;

// =================================================================================================
// The executor
// =================================================================================================

/// What `/control/ingest`'s batch id already means to this executor — **the three states, in the
/// order they are looked up** (contracts §3.4, whose Appendix R r8 names the third:
/// *"the batch-id idempotency rule acquires a third state in practice — held but not yet
/// acknowledged — which a retry must join rather than treat as new"*).
///
/// Computed by [`BatchState::of`] on the executor thread, never in a handler. `tessera-lifecycle`
/// deliberately does not know this type: the durable half lives on [`LiveState`], and the window
/// stays a container that knows nothing about idempotency policy.
pub(super) enum BatchState {
    /// Durably accepted: the WAL record is fsynced, the rows are applied and the ids are recorded.
    /// Same bytes replays these ids; different bytes is a `409`.
    ///
    /// The ids are carried rather than re-derived because **re-deriving is impossible** for a row
    /// that supplied no `external_id`: it is addressable only by its `tessera_id` (contracts §3.4
    /// r6) and appears in no map keyed by anything the retry sends.
    Accepted {
        body_hash: [u8; 32],
        entity_ids: Vec<EntityId>,
    },
    /// **Held but not yet acknowledged**: an entry of the *open* commit window. Its ids exist —
    /// allocation happens at the close — so there is nothing to replay yet; what a byte-identical
    /// retry gets is a place in the queue of waiters that entry will ack.
    ///
    /// `window_seq` identifies the window the entry was found in. There is exactly one open window
    /// and it is consulted and joined in the same statement, so this is read by a
    /// `debug_assert!` and nothing else; it is the discriminator an executor holding more than
    /// one window would need.
    Held {
        window_seq: u64,
        body_hash: [u8; 32],
    },
    /// Never seen. A new entry.
    ///
    /// **This state is reachable for a batch that was in fact accepted, and that is a live
    /// caveat.** `accepted_batches` is a cache of the WAL: it is rebuilt from replay
    /// ([`WritePath::reconstruct`]) and trimmed at rotation (`Executor::rotate_wal`), so once the
    /// member holding a batch's record is reclaimed its `batch_id` regresses to `Unknown` and a
    /// retry is re-ingested. Rows carrying an `external_id` are then caught by the duplicate check
    /// and the batch 409s; **rows without one are re-ingested silently as new entities**, leaving a
    /// second copy that no external id names and no deny can reach — the same unreachable duplicate
    /// the window's conflict check exists for, arrived at by retention rather than by a race.
    ///
    /// The horizon is therefore the retained log — about one flush, rotation reclaiming below the
    /// oldest unconsumed row — with or without a restart, which is the statement contracts §3.4
    /// makes to a client: a client that needs a longer horizon carries its own id column.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: **durable index first, then the open window, then unknown**.
    ///
    /// The two sets are disjoint — a batch id enters `accepted_batches` only at
    /// `close_window`, which consumes the window holding it, and a durably-accepted batch is
    /// refused before it can be pushed — so the order changes no answer today. It is still written
    /// durable-first, because the durable record is the one that survives a restart and an
    /// implementation that preferred the volatile half would answer differently on either side of
    /// one.
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
    /// It was answered outright — a replay, a join or a 409 — and nothing was added to the window.
    Answered,
    /// A conflicting external id forced the open window to close. The pass must **yield** to
    /// `Executor::run`'s deny drain (lifecycle §1.3).
    YieldedAfterClose,
}

/// The most entries one deny window may hold, and the most changes `/control/changes` enqueues
/// before it collects.
///
/// **It bounds two things and trades nothing.** Below it, a larger value only ever reduces fsyncs
/// and overlay clones; the only things a larger value costs are the size of a failed window's fold
/// and the pending receipts a caller holds. So this is not a knob an operator has a decision to
/// make about, and it is deliberately a constant rather than a configuration key.
///
/// The two bounds, in the order they bind:
///
/// 1. **The drain terminates.** Every entry the deny drain pulls is one a concurrent submitter can
///    replace, so a window with no bound need never close while denies keep arriving — and a window
///    that never closes is not a large window, it is no deny ever being acked. See
///    [`Executor::run_deny_pass`].
/// 2. **Pending receipts stay bounded.** `/control/changes` enqueues in chunks of this size, so the
///    deny runtime's pool (`DENY_MAX_BLOCKING_THREADS` handlers) holds at most that many times this
///    many one-slot channels, rather than that many times whatever fits in a request body.
///
/// One chunk covers the overwhelming majority of revocation requests, so the common case is one
/// window and one fsync. A maximal request body splits into a few tens of windows — against the
/// tens of thousands of fsyncs the per-item path charged for the same request.
pub const DENY_WINDOW_MAX_ENTRIES: usize = 1_000;

/// How many deny windows may pass before the overlay publishes regardless of whether the drain has
/// closed.
///
/// **A liveness floor, not a latency bound.** The drain loops while the deny lane is non-empty, so
/// under arrival faster than application it need never close, and without a floor the newest
/// manifest would trail live state indefinitely. Latency is not what this bounds — architecture §3
/// (r23) budgets the whole write path at seconds to minutes, denies included, and grants latitude
/// in *when* work is batched under one condition this design keeps: a deny's ack stays coupled to
/// its application, which happens at the window's own fsync and swap, upstream of any publication.
///
/// What the batching *does* buy is bytes. A side-manifest is complete state (contracts §2.3), so
/// publishing per window through a bulk revocation of `N` rewrites a growing set once per window —
/// Θ(N²/window) on disc. Collapsing a burst into one write removes that; this floor bounds the
/// exposure the collapsing admits, at ≤ 64,000 dispositions, all durable in the WAL, all enforced
/// live, and recovered by any WAL-bearing restart. Only a no-WAL restore sees the gap.
pub(super) const OVERLAY_PUBLICATION_MAX_WINDOWS: u64 = 64;

/// How often the executor re-checks for a completed flush while one is in flight or completed
/// but not yet drained.
///
/// **This is what bounds publication latency on an idle node, and it exists because the pool must
/// not ring the doorbell.** `flush_submit`'s doc records why: an executor holding a clone of its
/// own bell sender would keep the channel alive for ever and `WritePath::drop`'s join would hang —
/// and a pool task holding one re-creates the same hang for the duration of a flush at shutdown.
/// So the wake-up is a poll, armed only while [`ExecutorHealth`]'s `flush_in_flight` /
/// `flush_completed_pending` pair says there is something to wait for: a quiescent executor still
/// sleeps the full tick, and a flush's publication lands within this interval of its files being
/// durable rather than at the next tick — which is what keeps `POST /control/flush` "prompt" on
/// an idle node, and the ack→visibility bound at one tick rather than two.
pub(super) const FLUSH_COMPLETION_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How long after a cycle failed to publish the next retry may come.
///
/// A failed cycle stays open and re-arms its request (`ExecutorHealth::fail_publication_cycle`),
/// and an armed request puts [`Executor::wait_for_work`] on the completion poll, so the retry
/// would otherwise come fifty times a second for as long as an operator condition stands. One
/// second keeps a `wait=visible` caller's bounded wait worth making while leaving a shut gate
/// costing one plan and one log line a second at most. A period tick and a row trip are not held
/// back by it.
pub(super) const FAILED_CYCLE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// How often the executor looks for a **completed fold** while one is running.
///
/// Coarser than [`FLUSH_COMPLETION_POLL`] purely because a fold's duration is minutes to hours
/// rather than seconds (compaction §14: modelled, IO-bound, never measured), so the 20 ms interval
/// would spin the loop hundreds of thousands of times waiting for one publication. There is no
/// latency argument on the other side: compaction §6.1's standing ruling is that a fold's wall
/// clock is a property nobody observes, so a fifth of a second at the end of it is free. Once the
/// fold *has* sent, `fold_completed_pending` puts the wait back on the fast poll.
pub(super) const FOLD_COMPLETION_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// How long the executor waits before each re-attempt at making a deny window durable, and
/// therefore how many attempts there are: the first sync, plus one per entry here.
///
/// ## What bounds this, and why it is not the write-latency budget
///
/// Design §3's write-latency budget permits a deny to take seconds — up to a minute is acceptable —
/// so there is room. The bound is **not** taken from it, for a reason the budget does not express:
/// the executor is a single thread and the deny lane is FIFO, so this delay is paid by *every* deny
/// queued behind the failing window, not once by the caller who hit the failure. A schedule sized
/// to the budget would let one failing device convert the whole budget into the lane's per-window
/// cost, and the lane's guarantee — never starved beyond one window — is measured in exactly that.
///
/// So the bound is taken from the lane's own observed latency instead. A deny acks in ~3.2 ms
/// quiescent and 165 ms p50 / 346 ms max under sustained ingest
/// (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`, measured). At 250 ms of added delay a
/// failing window stays inside the range the lane already exhibits under load, so nothing queued
/// behind it waits longer than a busy node already makes it wait.
///
/// **Two re-attempts, not ten**, because of what the repair is: re-dirtying the pages and syncing
/// again (`tessera_lifecycle::wal::Wal::retry_durability`). That converts a transient writeback
/// error; it does nothing about a device that is actually failing. If the third attempt is refused,
/// further attempts are a cost with no mechanism behind them.
///
/// **The delays are not zero**, because the other failure a retry plausibly converts is a
/// short-lived `ENOSPC` — for which an immediate re-attempt is the one schedule guaranteed not to
/// help.
///
/// *Chosen against a measurement, not itself measured: no campaign has established how often a
/// second attempt succeeds, because that is a property of the device rather than of this code.*
pub(super) const DENY_DURABILITY_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(200),
];

/// How many durability attempts one deny window gets in total — the original sync plus one per
/// [`DENY_DURABILITY_BACKOFF`] entry.
///
/// Public because a test that wants to observe the *exhausted* path has to arm exactly this many
/// failures, and a test that hard-codes the number silently stops testing exhaustion the day the
/// schedule changes — it starts testing recovery instead, and passes either way.
pub const DENY_DURABILITY_ATTEMPTS: usize = DENY_DURABILITY_BACKOFF.len() + 1;

/// The levels a fold's retirement is about to move, and the set it retires.
///
/// A fold writes its manifest before it retires, because the retirement is not reversible and a
/// manifest that would not commit must leave it undone. So at step 3a the store holds the
/// pre-retirement records while the prefix being written holds the post-retirement ones. The
/// retirement changes a level in three ways: an artifact whose own entity is retired leaves (its
/// slot becomes a hole), a surviving artifact loses the retired members from its membership, and a
/// content whose generating set lost a member is dropped or shrunk.
///
/// For a row column and a tile index only the first matters. Both are the memberships projected
/// through the row space this fold wrote, and a retired entity has no row in it (pass 1 dropped
/// them), so a surviving artifact projects to the same rows before and after its membership
/// shrinks; a generating set is read from the store's records and from neither structure. So the
/// fold composes both from the store's records without the artifacts the retirement removes
/// ([`Self::records`]) and stamps them with the version the level will have once it has run
/// ([`Self::version_after`]). A containment partition is rank-sensitive, since a dropped content
/// shifts the ranks after it, and a spatial level's row forms are resolved from shapes the
/// retirement removes, so those stay omitted for a pending level and recompose on first use.
///
/// The version after the retirement is the store's plus one for a level here and the store's own
/// otherwise: `ArtifactStore::retire` moves a level it changes by one, and it reads the same
/// predicate `levels_moved_by` read to fill this. `publish_fold` checks the two agree after the
/// retirement and drops any structure whose stamp the store does not then carry.
///
/// What happens when the retirement does not follow the manifest. A fold discarded between the
/// manifest and the flip leaves a prefix `CURRENT` never names, which nothing opens. A process
/// that dies after the flip restarts from the manifest: its records are the post-retirement ones
/// and its stated version is the stamped one, so the structures describe what was seeded, and any
/// record the log replays over them moves the version and they are refused at the version check.
/// In every case the level recomposes on first use, at a cost and never with another level's
/// answer.
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
/// A file whose level has moved is dropped here, so a manifest never names one nothing could
/// adopt. For a level in `pending_retirement` (the fold's own case, see [`PendingRetirement`]) the
/// version is the store's plus one: what the level will carry once the retirement has run, and
/// what the fold stamped the files it composed for it with.
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

/// Every view the bundle holds, across partitions. A flush plans per view, because a segment's
/// entity range is contiguous only within one (§2.1).
/// Replace a manifest's deny fields with the overlay's live state.
///
/// **Serialised fresh at every write, never carried forward from another manifest.** A
/// side-manifest is complete current state (contracts §2.3), and the two fields are the only ones
/// whose truth lives outside the files the manifest names — so copying them from the manifest
/// being extended would publish whatever was true when *that* one was written, indefinitely, and
/// an unsuppress would never reach disc. The rule is one line here and it is the whole of what
/// keeps a manifest a projection of live state rather than an input to the next one.
///
/// The two fields are taken from the two bitmaps separately, never from `Overlay::denied`'s union:
/// they retire under different rules (lifecycle §3), and publishing the union under one field
/// would make every deletion look retirable by an unsuppress.
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

/// Carry the live vocabulary bindings into a manifest's `vocabulary_extensions` — `write_deny_state`'s
/// sibling, called beside it at every publication site except the fold's.
///
/// **Union, never restate.** `manifest` here is always `partition_data.manifest.clone()`, so it
/// already carries every extension a previous publication wrote; `extensions_beyond` gives only
/// what `MANIFEST.vocabularies` (the *build*, not this side-manifest) does not already carry, and
/// this appends that into what is already held rather than replacing it. That asymmetry with
/// [`write_deny_state`] is deliberate and is [`tessera_store::manifest::VocabularyExtension`]'s own
/// documented rule: `deny` must be able to shrink on an unsuppress, so it is restated fresh every
/// time; a binding must never shrink, so restating it fresh is exactly the shape that could
/// silently drop one. On the executor as it stands today `vocabularies` is always the same live
/// generation the manifest was cloned from, so `extensions_beyond` happens to recompute a superset
/// of whatever is already held — but that is a fact about today's single-threaded caller, not a
/// property of this function, and this function must hold even if that caller ever changes. The
/// unit test beside it proves the union rather than trusting the coincidence.
///
/// The fold does not call this: it folds every served `vocabulary_extensions` directly into the new
/// prefix's `MANIFEST.vocabularies` and writes an empty extension set on purpose (see
/// `publish_fold`) — restating the same bindings here as well would bind each key twice, once in
/// each home.
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

    /// **A carried binding must survive even when the live view has nothing to say about it.**
    /// `extensions_beyond` only emits an entry for a vocabulary its own `by_name` tracks
    /// (`vocabulary.rs`'s doc on the type), so a manifest that already carries an extension for one
    /// the live view does not — here, `vocabularies` tracks only `"department"`, with no bindings of
    /// its own, so `extensions_beyond` returns nothing at all — must not have that carried entry
    /// erased by a write that touches an unrelated vocabulary.
    ///
    /// **Mutation:** replace the union body with
    /// `manifest.vocabulary_extensions = vocabularies.extensions_beyond(bundle_vocabularies);` and
    /// this fails — the carried `"legacy"` binding is wiped by a write that had nothing new to say.
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

    /// The ordinary case beside it: a fresh mint is appended beside what is already carried, and a
    /// binding restated identically is not duplicated.
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
        // Restates the binding the manifest already holds, plus one genuinely new one.
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

/// The one plan a dispatch sends, chosen by **oldest unflushed row**.
///
/// Free and pure so the choice can be tested without an executor — and it is the choice, not the
/// dispatch, that carries the property. See `Executor::dispatch_flushes` for why one plan.
pub(super) fn plan_to_dispatch(
    plans: Vec<(String, crate::flush::FlushPlan)>,
) -> Option<(String, crate::flush::FlushPlan)> {
    plans.into_iter().min_by_key(|(_, plan)| {
        // `items` is ascending by entity id and I9 issues ids monotonically, so the first is this
        // view's oldest waiting row. An empty plan cannot occur (`plan_flush` returns
        // `NothingToFlush`), and sorting it last rather than first keeps a hypothetical one from
        // winning every tick.
        plan.items
            .first()
            .map_or(u64::MAX, |(entity, _)| entity.raw())
    })
}

/// Whether a completed flush's dictionary moved under it — see the call site in
/// [`Executor::publish_flush`] for the argument, and the scoping this encodes.
///
/// Pure so the **scoping** is testable: a flush that promoted nothing (`None`) is never discarded
/// however far the dictionary has moved, because its tier names only ordinals below the length it
/// planned against and append-only extension preserves those. Broadening this to every flush would
/// be a liveness hole bought for no safety.
pub(super) fn dictionary_moved_under(promoted_from_dict_len: Option<u32>, live_len: u32) -> bool {
    promoted_from_dict_len.is_some_and(|planned| planned != live_len)
}

/// One superseded prefix awaiting reclamation, and the two `Arc`s whose release says no thread can
/// still resolve a path inside it — see [`Executor::pending_reclaim`].
pub(super) struct PendingReclaim {
    generation: Arc<Generation>,
    prefix_dir: PathBuf,
    /// Every sidecar that was live over this prefix **before** the one the held generation carries
    /// — see [`Executor::superseded_sidecars`].
    superseded_sidecars: Vec<std::sync::Weak<crate::session::ExternalIdIndex>>,
}

/// Seconds since the Unix epoch, or `None` if the clock is before it.
///
/// `None` reads as "no fold has ended yet", which switches the interval floor off rather than
/// jamming it on — the safe direction for a clock this absurd, and the same answer a fresh process
/// gives.
pub(super) fn unix_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// The `(layer, level)` a record changes the artifacts of, and `None` for every other record —
/// what a caller needs to read that level's version before the record moves it.
pub(super) fn artifact_level_of(record: &WalRecord) -> Option<(&str, u32)> {
    match record {
        WalRecord::ArtifactPublish { layer, level, .. }
        | WalRecord::ArtifactGrow { layer, level, .. }
        | WalRecord::ArtifactFill { layer, level, .. } => Some((layer.as_str(), *level)),
        _ => None,
    }
}

/// **The growth records one closed window owes**, with the index of the entry to blame if an append
/// fails, in the order they are to be appended.
///
/// **One record per `(layer, level)` for the whole window, not one per entry.** A record is a list
/// of `(ordinal, joining)`, entries in a window are already committed together under one fsync, and
/// several batches naming one cluster are the ordinary shape of a client ingesting in parallel — so
/// merging costs one union and saves a record and a pin per batch. What may not merge is the
/// address: a join carries its own `(layer, level, ordinal)` and nothing infers one from another's.
///
/// The entities are `entity_ids[row]` — the assignment this window just made, in the caller's own
/// row order (`tessera_lifecycle::ClosedEntry`) — which is what puts a point's membership in the
/// same commit as the point.
pub(super) fn growth_records<W>(closed: &[tessera_lifecycle::ClosedEntry<W>]) -> Vec<(WalRecord, usize)> {
    use std::collections::BTreeMap;
    /// One `(layer, level)`'s joins: the entry to blame for the append, and a bitmap per ordinal.
    pub(super) type Level = (usize, BTreeMap<u32, croaring::Bitmap>);
    // Ordered, so the records a window appends do not depend on hash iteration order: two nodes
    // replaying one log must read the same sequence, and a test comparing two runs is entitled to
    // the same one.
    let mut by_level: BTreeMap<(&str, u32), Level> = BTreeMap::new();
    for (index, entry) in closed.iter().enumerate() {
        for join in &entry.memberships {
            // A key with no ordinal was minted at the close, and a minted artifact was published
            // *carrying* these rows — one record instead of a publication and a growth against it.
            let Some(ordinal) = join.ordinal else {
                continue;
            };
            let (_, ordinals) = by_level
                .entry((join.layer.as_str(), join.level))
                .or_insert_with(|| (index, BTreeMap::new()));
            let joining = ordinals.entry(ordinal).or_default();
            for row in &join.rows {
                let entity = entry.entity_ids[*row as usize];
                // Entity space is `u32` by I9, so the narrowing is total.
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

/// **The artifacts a closed window's rows named and no artifact holds** — the records that create
/// them, in the order they must be appended, and how many each entry is to be told it created.
///
/// **Minting is a publication, and it happens here rather than at admission.** An ordinal is claimed
/// from the level's own cursor and is durable only in the record that claims it, so a claim made at
/// admission would be held, unappended, across everything the executor does before the window
/// closes — including a `PublishArtifacts` command, which reads the same cursor and would take the
/// same ordinal. Here there is nothing to interleave with: the window is closed, the allocation is
/// made, and the record is appended a few statements later inside the window's own fsync.
///
/// **One artifact per key per level, for the whole window** (`artifacts-from-points.md` §5's second
/// ruling). The keys are gathered into one map before anything is prepared, so two points in one
/// batch — or two batches in one window — naming the same unknown key mint once and join the one
/// artifact. A key a *live* artifact already holds is not minted at all: it is re-resolved here
/// against `ArtifactStore::ordinal_of_key`, because a publication may have landed between the
/// batch's admission and this close, and it grows instead.
///
/// **A minted artifact is published carrying its members**, not published empty and then grown. The
/// entities exist by this point — the allocation is the statement above the caller — so the one
/// record says the whole of what happened, and the join needs no second record and no log pin of
/// its own. That is why `growth_records` skips a membership whose ordinal is `None`.
///
/// **The edges are created, and this is the one route by which the wire creates one.** A growth adds
/// members and never lineage, so a lineage naming an artifact that already exists can only be
/// checked; a lineage naming one that does not yet exist is settled where every edge is settled, at
/// the publication that creates the artifact. The chain arrives **parent before child** — the
/// ordering constraint `annotation-representation.md` §5.0.4 puts on edges, applied to a batch — in
/// the only two shapes a column can spell: a nested lineage is one level and one record, where
/// `prepare_publish` resolves a sibling's ordinal within its own batch whatever order the artifacts
/// sit in; and a tiered chain is a record per level, coarse first, where the parent's ordinal was
/// fixed by the record before and is answered by `pending`.
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
                // **The first entry that named the key owns the mint**, which is what makes the
                // per-batch count sum to the window's: `growth_records` blames an append the same
                // way, and one convention for both keeps a report from double-counting.
                .or_insert_with(|| (index, croaring::Bitmap::new()));
            for row in &join.rows {
                // Entity space is `u32` by I9, so the narrowing is total.
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

/// **A key that acquired an artifact between its resolution and its preparation grows into it**,
/// rather than minting a second artifact for a key a live one already holds.
///
/// [`Executor::prepare_mints`] re-resolves every key it is given against the store, and answers
/// the ones that turned out to be held; this writes those ordinals back onto the memberships, so
/// [`growth_records`] and [`values_growth_records`] carry them as ordinary joins. A membership
/// left with no ordinal is one the preparation is about to mint, and its publication carries the
/// rows.
///
/// **It can only find something at the ingest door.** There, a window stays open across a
/// `PublishArtifacts` command, which takes the work lane between an entry's admission and the
/// window's close. At the values door the resolution and the preparation are two statements of one
/// executor call with nothing between them, so `resolved` is always empty and this is a no-op —
/// kept rather than elided because the two doors settle a batch the same way, and a door that
/// skipped it would be the one to get this wrong if the call ever grew a yield.
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

/// A second is short against the interval an operator or an orchestrator would take to notice, and
/// long enough that a genuinely dead device is retried sixty times a minute rather than continuously.
///
/// It is not a latency bound on anything a caller sees: a degraded node still answers denies
/// immediately, and traffic arriving at any point wakes the loop through the doorbell as usual, so
/// a busy node attempts recovery far more often than this.
pub(super) const WAL_RECOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One deny in an open window: its record, and everything needed to apply it and answer its caller.
///
/// `record` is built at the drain rather than at the append so the window is a list of things that
/// are ready to be written — the append loop does no work that can be got wrong per entry.
pub(super) struct DenyEntry {
    record: WalRecord,
    entity: EntityId,
    op: ChangeOp,
    /// The waiter, or `None` for an entry nobody asked for.
    ///
    /// **`None` is the cascade** (`Executor::cascade_dependents`): deleting an artifact deletes
    /// the artifacts depending on it, and those deletions have no caller to answer. They are
    /// entries in every other respect — their own WAL record, applied in the same window, retired
    /// at the same fold — so the ack is the only thing that distinguishes them.
    respond: Option<Responder>,
}

/// The single writer. One per partition, on its own thread, owning the WAL by value.
pub(super) struct Executor {
    pub(super) wal: ExecutorWal,
    pub(super) live: Arc<LiveState>,
    /// **The only publishing capability in the write path.** Not in [`LiveState`], which the
    /// handler side shares.
    pub(super) generation: Arc<GenerationHandle>,
    /// The row-projection cache, shared for the one thing this thread does with it: dropping the
    /// projections of generations now older than the retention depth. Runs at the swap — see
    /// `RowProjectionCache::prune_generations_below`.
    pub(super) row_projection_cache: Arc<RowProjectionCache>,
    /// See [`MaintenanceDeps::region_cache`].
    pub(super) region_cache: Arc<
        crate::single_flight::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// The artifact row forms — rebuilt here at the fold, and read by every viewport. See
    /// [`MaintenanceDeps::artifact_projections`].
    pub(super) artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// See [`MaintenanceDeps::shapes`].
    pub(super) shapes: Arc<crate::shapes::ShapeStore>,
    /// The lineages, rebuilt beside them and for the same reason.
    pub(super) lineages: Arc<crate::cut::Lineages>,
    /// The supplied-content tables, held for the layer drop below. Not warmed at the fold: a
    /// table is read from the blob the fold has just rewritten, and reading every level's is a
    /// pass over the whole of it — where a row form is rebuilt there because row space renumbered
    /// under it, this one is merely stale and the first request that wants a level pays for that
    /// level alone.
    pub(super) level_contents: Arc<crate::artifact_content::LevelContents>,
    pub(super) queues: LifecycleQueues,
    pub(super) health: Arc<ExecutorHealth>,
    /// The last window's sequence number. [`BatchState::Held`] is what it is for; all it has to be
    /// is distinct per window.
    pub(super) window_seq: u64,
    /// §4's `flush_max_age_secs` — the tick's period.
    pub(super) flush_max_age_secs: u64,
    /// §4.1's `flush_max_items` — buffered rows at which the tick comes due ahead of its period.
    pub(super) flush_max_items: usize,
    /// Distinguishes two flush attempts at the same `segments_version` — see the `seg_id` this
    /// feeds.
    pub(super) flush_attempt: u64,
    /// The next `SEGMENTS-<n>.json` number. Every writer of a side-manifest takes its number here,
    /// at the moment it writes: a number taken when a flush is planned is stale by the time it
    /// lands. [`Executor::allocate_manifest_n`] also raises it over the files on disc, because a
    /// counter only knows what this executor wrote.
    pub(super) next_manifest_n: u64,
    /// Whether live state holds something no side-manifest carries yet. Cleared only by a
    /// successful publication, so a node that was poisoned or diverged publishes once on recovery.
    pub(super) deny_dirty: bool,
    /// Deny windows applied since the last publication — the counter
    /// [`OVERLAY_PUBLICATION_MAX_WINDOWS`] floors.
    pub(super) windows_since_publication: u64,
    /// The bundle root, not the prefix directory. A fold moves the prefix, so
    /// [`Executor::prefix_dir`] derives the directory from the live generation at each use.
    pub(super) bundle_root: PathBuf,
    pub(super) identity_key: IdentityKey,
    /// The shared compute pool a flush executes on (§1.1), and the handle it submits its completed
    /// unit back through.
    pub(super) pool: Arc<rayon::ThreadPool>,
    /// See [`MaintenanceDeps::max_distinct_terms`].
    pub(super) max_distinct_terms: u64,
    /// Completed flushes arriving from the pool. Its own channel: a completed flush's files are
    /// durable and may not be shed, and a pool task cannot hold the handle. Drained after the deny
    /// lane, so a suppression never queues behind a flush's publication.
    pub(super) flush_done: Receiver<crate::flush::CompletedFlush>,
    /// The entity-space coalesce's policy, its in-flight flag, its attempt counter and its own
    /// completion channel — the same three-part shape a flush has, and separate from a flush's for
    /// the reason decision 0044's D2 gives: the two halves of merge are independent work, and
    /// coupling them would make the cheap one wait on the expensive one.
    pub(super) coalesce_policy: crate::coalesce::CoalescePolicy,
    pub(super) coalesce_in_flight: Arc<AtomicBool>,
    pub(super) coalesce_attempt: u64,
    pub(super) coalesce_done: Receiver<crate::coalesce::CompletedCoalesce>,
    pub(super) coalesce_submit: Sender<crate::coalesce::CompletedCoalesce>,
    /// The background refresh's dependencies — see [`crate::refresh`].
    pub(super) refresh: crate::refresh::RefreshDeps,
    /// The row-space merge's policy, its in-flight flag, its attempt counter and its own
    /// completion channel. Separate from both the flush's and the coalesce's: a merge publishes as
    /// **its own swap** (decision 0044's D3 — the one-cadence rule lost its justification when pin
    /// retention was deleted, and under 0043 the coupling is harmful, since it makes a flush's
    /// zero-cost path carry the merge's refresh).
    pub(super) coalesce_enabled: Arc<AtomicBool>,
    pub(super) merge_policy: MergePolicy,
    pub(super) merge_enabled: Arc<AtomicBool>,
    pub(super) merge_in_flight: Arc<AtomicBool>,
    pub(super) merge_attempt: u64,
    pub(super) merge_done: Receiver<crate::merge::CompletedMerge>,
    pub(super) merge_submit: Sender<crate::merge::CompletedMerge>,
    /// The compaction fold's in-flight flag. A fold runs on its own thread, not the shared pool:
    /// it takes minutes to hours and the pool serves viewports.
    pub(super) fold_in_flight: Arc<AtomicBool>,
    pub(super) fold_attempt: u64,
    pub(super) fold_done: Receiver<crate::compact::CompletedFold>,
    pub(super) fold_submit: Sender<crate::compact::CompletedFold>,
    /// **The suggestion index's rebuild**, on the same in-flight / channel shape as the three
    /// passes above, and deliberately the *smallest* of them: it reads a vocabulary out of the
    /// generation and writes files the manifest does not name, so it has no plan, no gate and
    /// nothing to refuse. One at a time across every vocabulary, because the cost it exists to
    /// bound is the sort's memory and not its latency (`crate::suggest`).
    pub(super) suggest_dir: PathBuf,
    pub(super) suggest_in_flight: Arc<AtomicBool>,
    pub(super) suggest_build: u64,
    pub(super) suggest_done: Receiver<crate::suggest::CompletedSuggest>,
    pub(super) suggest_submit: Sender<crate::suggest::CompletedSuggest>,
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
    /// attempt then does, so the interval limits attempts: several discard causes are persistent,
    /// and each discarded fold leaves a whole prefix on disc. See [`Executor::fold_floor_from`].
    pub(super) last_fold_start_unix: Option<u64>,
    /// Every external-id sidecar replaced over the live prefix, weakly. A coalesce builds a new
    /// sidecar over the same prefix, so a generation still holding the old one is invisible to the
    /// strong counts [`Executor::reclaim_superseded_prefixes`] reads; a `Weak` answers whether one
    /// is still alive without keeping its mappings. Moved into the [`PendingReclaim`] at a fold.
    pub(super) superseded_sidecars: Vec<std::sync::Weak<crate::session::ExternalIdIndex>>,
    /// Every membership extent this node has published: the complete list, not a diff. Held here
    /// because a publication starts from a clone of the live generation's manifest, which a
    /// side-manifest write does not refresh; extending the clone would drop earlier entries.
    pub(super) membership_extents: Vec<tessera_store::manifest::MembershipExtent>,
    /// Every derived file the current prefix holds. Held here because a publication clones a
    /// manifest that may be stale. What reaches a manifest is this list filtered to the files the
    /// store's level versions still make adoptable ([`artifact_coordinates`]); the fold replaces
    /// it wholesale, its paths being relative to the prefix the fold publishes.
    pub(super) derived_extents: Vec<tessera_store::manifest::DerivedExtent>,
    /// Every artifact **content** extent this node has published, complete current state, held for
    /// the reason above and written the same way. The two lists travel together: a membership
    /// without its content leaves an artifact whose description cannot be read, which withholds it.
    pub(super) artifact_record_extents: Vec<tessera_store::manifest::RecordExtent>,
    /// Superseded prefixes awaiting reclamation, each held by the generation that named it. A
    /// prefix is deleted only once nothing else holds that generation or its external-id sidecar,
    /// because the sidecar opens its files lazily and a request could still be about to. A process
    /// that exits first leaves the tree for the startup sweep.
    pub(super) pending_reclaim: Vec<PendingReclaim>,
    /// The sender pool tasks are given a clone of. Nothing rings the doorbell when a flush
    /// completes: it is picked up at the next tick, and an executor holding its own doorbell sender
    /// would never see the disconnect that shuts it down.
    pub(super) flush_submit: Sender<crate::flush::CompletedFlush>,
    /// When the last tick fired. Started at construction, so the first tick is one period after
    /// the executor starts rather than immediately at startup.
    pub(super) last_tick: std::time::Instant,
    /// What every accepted write since the last tick did to each level's row forms, applied at the
    /// next tick. One level's deltas carry consecutive level versions. A level the fold rewrites
    /// has its entry dropped with its forms.
    pub(super) pending_forms: std::collections::BTreeMap<(String, u32), Vec<crate::artifacts::LevelDelta>>,
    /// The WAL's sequence position after the last rotation (or at start), so a tick can tell
    /// whether the log has grown since — the deny-only regime's rotation trigger (owner-ruled
    /// 2026-08-04; write-path §4.5). An idle node whose position has not moved rotates nothing.
    pub(super) wal_position_at_last_rotation: u64,
    /// When [`Executor::sample_wal_gauge`] last began a walk, or `None` before the first one.
    /// The rate limit on that walk is stated there; this is the clock it reads.
    pub(super) last_wal_sample: Option<std::time::Instant>,
    /// Walks [`Executor::sample_wal_gauge`] has taken, published as [`WalGauge::samples`] so a
    /// reader can tell a reading that was refreshed from one the rate limit held back.
    pub(super) wal_samples: u64,
    #[cfg(feature = "fault-injection")]
    pub(super) faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

/// The parent each child in these edges is named under, refusing a child named under two.
///
/// A list column declares the edges, so two rows naming different parents for one artifact are two
/// hierarchies and which of them was published would be the order the rows arrived in. The child is
/// keyed by its own level, which a levelled taxonomy needs: one key legitimately sits at two levels
/// and carries a different parent at each.
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
    /// Drop the region decompositions of generations older than the retention depth — the same
    /// pass, at the same swap, as `RowProjectionCache::prune_generations_below`.
    pub(super) fn prune_region_cache(&self, segments_version: u64) {
        let floor = segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS);
        self.region_cache
            .retain_keys(|key| key.segments_version >= floor);
    }

    /// Drain deny to empty, then execute **at most one** work item, then repeat — blocking only
    /// once both queues have been *observed* empty.
    ///
    /// That last clause is what makes the capacity-one bell safe. If the loop blocked while work
    /// remained, a token discarded as `Full` could be the only wake-up a queued job ever had. As
    /// written, a token is only ever discarded while a job is still visible to the `try_recv`
    /// below, so no job can be left asleep.
    ///
    /// **Deny priority** (lifecycle §1.3): deny is drained to empty at the top of every iteration
    /// and [`Executor::run_work_pass`] returns as soon as it closes a window, so a deny's wait is
    /// bounded by the window in front of it — at most two, see that function for the case — rather
    /// than by queue depth. The consequences are chosen: a sustained
    /// deny flood starves ingest completely, and the deny queue is unbounded in memory.
    ///
    /// **Why a deny may safely overtake a queued ingest.** Reordering execution relative to
    /// submission looks like it should break replay equivalence (a live `suppress` replaying before
    /// the ingest that established its target). It cannot: `/control/changes` resolves its
    /// `external_id` against the live map in the *handler* and 404s if the item is not established
    /// yet, and an item is established only at apply. So no deny naming a still-queued ingest's
    /// item can be submitted at all, and WAL append order still equals apply order.
    ///
    /// **The cost of that, stated because it is a real operator-visible gap and nothing closes it.**
    /// An operator issuing `suppress D` while D's ingest is still held gets **404 unknown external
    /// id**; D then becomes visible, unsuppressed, and the operator has to notice and retry. The
    /// commit window widened that interval from one command's fsync to a whole window. Putting deny
    /// dispositions in the window would not close it either — the deny is refused in the handler and
    /// never reaches a lane — so this is not an argument for the mixed window; it is an argument for
    /// an operator who is revoking during a bulk load to verify rather than to trust a 404.
    ///
    /// **Shutdown drains and executes; it does not discard.** The loop leaves only from
    /// `bell.recv()`, which sits *after* both `try_recv`s, so the disconnect iteration has already
    /// drained deny to empty and run one work item. Anything genuinely left behind — work queued
    /// beyond that one item — is dropped with the receivers, and had no waiter left to ack anyway:
    /// a submitter holds `&self` on the handle for the whole call, so `bell.recv()` cannot return
    /// `Err` while any submit is in flight, since the three senders live in one struct and
    /// disconnect together.
    ///
    /// "At most one work item" is one *pass*, and a pass drains work into a commit window
    /// ([`Executor::run_work_pass`]). Leftover doorbell tokens stay harmless under that: the drain
    /// takes every job visible to its `try_recv`, so a token is still only ever discarded while a
    /// job is still visible.
    pub(super) fn run(&mut self) {
        // The WAL gauge is sampled at the tick, and the first tick is a whole period away. Taken
        // once here so a node that has just restarted onto a log it replayed does not publish
        // "no members, no bytes" for that period, which reads as an empty log rather than an
        // unsampled one. This is the sample that arms the rate limit.
        self.sample_wal_gauge();
        loop {
            self.recover_wal();
            // **Completed flushes are applied before the tick plans another**, and the order is
            // load-bearing: until a flush is published its items are still in the buffer, so a tick
            // that planned first would re-plan the very rows the completed unit already wrote.
            let published = self.publish_completed_flushes()
                | self.publish_completed_coalesces()
                | self.publish_completed_merges()
                | self.publish_completed_folds()
                | self.publish_completed_suggests();
            self.tick_if_due();
            while self.run_deny_pass() {}
            // **At drain close**: one write covers a burst of consecutive windows rather than one
            // per window, which is what keeps a bulk revocation from rewriting a growing complete
            // state once per 1,000 entries. Runs on every iteration, so a node whose publication
            // was refused while poisoned publishes as soon as it recovers — the wait below is
            // tick-bounded, so that is within one tick even on an idle node.
            self.publish_overlay_state();
            if self.run_work_pass() || published {
                continue;
            }
            if !self.wait_for_work() {
                break;
            }
        }
    }

    /// **The flush tick** (§1.3): the one cadence on which geometry is published.
    ///
    /// Runs at the top of the loop, *before* the deny drain, so a tick is never delayed by work
    /// that arrived after it came due — and after it, because a tick that publishes must not
    /// preempt a deny already queued (lifecycle §1.3's priority lane).
    ///
    /// **Three triggers reach this cadence and none publishes off it.** The period, the
    /// buffered-row count, and `POST /control/flush` — accepted at any time and executed here, its
    /// 202 already meaning "accepted, not yet done".
    ///
    /// **The row trigger is what bounds the commit window's cost** (write-path §4.1). Every close
    /// deep-copies the buffer, so with `B` rows buffered between publications and a close every `W`
    /// a flush interval pays `B²/2W` — and under the age tick alone `B` is the arrival rate times
    /// the period, unbounded in the rate. `flush_max_items` bounds `B` directly, which is the axis
    /// `docs/evidence/memos/2026-08-05-ingest-rate.md` measures an interior optimum on. (This is
    /// decision 0045's deleted key, restored 2026-09-04 with a consumer: the earlier one marked
    /// the buffer "flush-ready" and nothing read the mark, because the tick never skipped a
    /// non-empty buffer either.)
    ///
    /// It also drives `reclaim` — lifecycle §2.1 assigns that gap to "whichever stage introduces
    /// a periodic publisher", and this is that publisher.
    pub(super) fn tick_if_due(&mut self) {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        // **A requested flush pulls the deadline forward; it does not publish off the cadence.**
        // `POST /control/flush` sets the flag and rings the doorbell, and the tick fires here, on
        // this one path, at the next loop iteration — so everything a tick guarantees (one flush
        // in flight, plan gates, rebase, retention) holds for an operator-triggered flush exactly
        // as for a scheduled one. **What keeps the publish-on-trip hazard that killed
        // `flush_max_items` away is now the caller.** An operator's `POST /control/flush` has a
        // rate of its own, unrelated to ingest. A write sent with `wait=visible` (contracts §3.4)
        // requests a tick too, so a loader that set the parameter on every page would publish
        // once per page, which is the publication period proportional to ingest rate that
        // decision 0045 removed, and would rotate every session's projection key at its own send
        // rate. The parameter is for a single writer reading back what it just wrote; a loader
        // sends its pages without it and one flush at the end.
        // **The occupancy the executor itself maintains**, not a count derived from a generation
        // this thread would have to load: `apply_window` and every flush publication store it, so
        // the trigger reads the same figure `/control/ingest`'s 429 is checked against.
        let rows_due = self.health.buffered_items.load(Ordering::SeqCst) >= self.flush_max_items;
        // **A row trip is a tick**, with the period restarted under it — not a second cadence
        // beside the period. That is what stops the two compounding: a loader fast enough to trip
        // the rows publishes on the rows and the age clock never comes due, and a loader slow
        // enough never to trip them publishes on the age exactly as before.
        let period_due = self.last_tick.elapsed() >= period;
        let due = period_due || rows_due;
        let requested = self.health.flush_requested.load(Ordering::SeqCst);
        let fold_requested = self.health.fold_requested.load(Ordering::SeqCst);
        if !due && !requested && !fold_requested {
            return;
        }
        // **A re-armed retry is floored** ([`FAILED_CYCLE_RETRY`]). The request an unpublished
        // cycle re-armed is the same flag a caller sets, so a tick fired by one while a failure
        // is outstanding is a retry, and retrying at the completion poll's rate would re-plan the
        // buffer fifty times a second for as long as the condition stands. A period tick and a
        // row trip come through regardless, which is what stops the floor from delaying ordinary
        // publication.
        if !due && self.health.failed_cycle_backoff().is_some() {
            return;
        }
        // **At most one flush in flight**, read once here and not again below, because the
        // publication cycle turns on it: a tick that will skip publishes into the cycle the
        // running flush already opened, and a tick that will go on to dispatch opens one of its
        // own before it publishes anything. Two reads could disagree. The value goes stale only
        // in the direction of a flush having landed, which costs the skipped tick nothing it did
        // not already risk.
        let flush_in_flight = self.health.flush_in_flight.load(Ordering::SeqCst);
        if !flush_in_flight {
            // **The cycle opens before anything is published**, and consumes the flush request it
            // honours in the same lock (`ExecutorHealth::open_publication_cycle`). Everything
            // below, the row forms and a flush that lands on the pool alike, belongs to this
            // cycle, and its number is not reached until the last of it is applied.
            self.health.open_publication_cycle();
        }
        // **The WAL's size and its rotation bound, sampled here because nothing off this thread
        // can read them.** The log is owned by the executor and a status request has no route to
        // it, so the gauge is taken at the tick and published as of that tick. Before this tick's
        // own publication, so the reading is what the tick found rather than what it left; on both
        // the flushing and the flush-skipped path, so a node whose flush is stalled still reports
        // the log growing under it. The tick is not a period — a row trip fires one every
        // `FLUSH_COMPLETION_POLL` while the buffer is full — so the walk rate-limits itself to one
        // per period and returns without doing anything on the ticks in between.
        self.sample_wal_gauge();
        // **Reclamation is checked at every tick, ahead of the flush's in-flight gate**, because it
        // is the one maintenance step whose readiness depends on nothing this executor does: it is
        // waiting on request threads to finish against a superseded generation. Skipping it on a
        // tick that found a flush running would leave a whole prefix on disc for another period
        // for no reason.
        self.reclaim_superseded_prefixes();
        // On the same argument, and ahead of the flush's in-flight gate for the same reason: it is
        // owed to residency rather than to any request, and it waits on nothing this thread does.
        self.dispatch_suggest_rebuild();
        // **The row forms of every level a write touched, published here and nowhere else**
        // (`ingest.md` §1.3, §10 ruling 6). Ahead of the flush's in-flight gate on reclamation's
        // argument: it waits on nothing this thread does, and a tick that found a flush running
        // still owes the interval's writes their publication. A request builds no form, so a level
        // whose deltas are not yet published is served as last published — up to a tick stale,
        // with counts understating and never the reverse.
        self.publish_row_forms();

        let generation = self.generation.load_full();

        // **At most one flush in flight, checked before any plan is built.** A period tick
        // arriving while one runs is *skipped, not queued* — two concurrent flushes would
        // double-consume the buffer range — and a skipped tick must not pay the plan either: a
        // plan deep-clones every buffered item, which at the buffer bound is an O(buffer-bytes)
        // allocate-and-free for a gauge (memory review, 2026-08-04). The gauge is fed from a
        // clone-free count instead, so a stalled flush still shows its backlog growing. Skips are
        // counted and alarmed, because a flush persistently slower than the tick is a
        // visibility-latency breach that `flush_max_age_secs` would otherwise silently miss.
        //
        // **A *requested* flush is not consumed by a skip.** The flag stays armed and a
        // requested-only wake returns without counting a tick, so the request executes at the
        // first iteration after the in-flight flush lands — which `FLUSH_COMPLETION_POLL` bounds
        // to within ~20 ms of its publication. Consuming it here would silently drop an
        // operator's "drain now" whenever it raced a scheduled flush.
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
                // **A skipped row trip is not the alarm the skipped period is.** The row
                // trigger asks for a publication as soon as `flush_max_items` have buffered, and
                // a loader fast enough will ask again while the last one is still writing — that
                // is the trigger doing its job under backpressure, and the buffer is bounded by
                // `ingest_buffer_max_items`'s 429 whatever happens. A missed *period* is the
                // visibility-latency breach, because it is the guarantee `flush_max_age_secs`
                // makes. Counting both here would bury the one alarm under the other's noise:
                // a 36M-row cell logs a few hundred row trips against a flush and none of them
                // is a breach.
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
        // A requested flush was consumed at the open above, whether or not there is anything to
        // flush: a `POST /control/flush` against an empty buffer is satisfied by the tick it
        // triggered rather than held until something arrives.

        // **Planned on this thread, executed on the pool.** The plan — which buffered items
        // acquire geometry and what the three dispositions do to them (§3.5) — is the
        // invariant-bearing half and is taken against the live generation here; the segment write
        // and the publication follow through `dispatch_flushes`. The count it produces on the way
        // is what an operator needs to see a stalled flush: items that *would* acquire geometry at
        // this tick, which stays at zero on a gated node and grows on one whose flush is failing.
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
                    // Once per period. The refusal stands until an operator acts, and the cycle
                    // it holds open re-arms its request, so the tick comes round at the retry
                    // floor.
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
            // Nothing to flush, so nothing is left over either: a view whose rows stopped being
            // flushable takes its plan out of the running, and a flag left standing would hold
            // the cycle open with nothing coming to close it.
            self.health.deferred_plans.store(false, Ordering::SeqCst);
            // Nothing to flush, so no publication is coming to rotate the log — the deny-only
            // regime. See `rotate_if_grown`.
            self.rotate_if_grown();
            if gated {
                // **A refused plan leaves an unpublished cycle.** The rows are still buffered and
                // the overlay still holds what a publication would have carried, so the counter
                // must not move past them: the cycle stays open and the request stays armed until
                // the refusal clears (`ExecutorHealth::fail_publication_cycle`).
                self.note_publication_failure();
            } else {
                // Every view answered "nothing buffered", so this cycle's publication is the
                // empty one and everything it was asked to publish is served.
                self.health.close_publication_cycle();
            }
        } else if !self.dispatch_flushes(&generation, plans) {
            // Every plan was dropped before it reached the pool, so this cycle published nothing
            // it was asked to: it stays open and its request stays armed.
            self.note_publication_failure();
        }
        // **The fold is dispatched before the two it suspends**, so a tick that starts one does not
        // also start a merge that the flip would orphan (compaction §1).
        self.dispatch_fold(&generation);
        // **The entity-space coalesce shares the tick and nothing else** (decision 0044 D2). It
        // is independent of the flush: it consumes what earlier ticks published, so a tick that
        // dispatched a flush may dispatch one too, and a gated node — which publishes no
        // geometry — still bounds the axes a coalesce owns.
        self.dispatch_coalesce(&generation);
        self.dispatch_merge(&generation);
        drop(generation);
    }

    /// Record that this cycle published nothing it was asked to publish.
    ///
    /// A thin wrapper so every site that drops a plan reads the same, and so the one rule stays
    /// in one place: the cycle stays open, the request is re-armed, and the retry is floored.
    pub(super) fn note_publication_failure(&self) {
        self.health.fail_publication_cycle();
    }

    /// Whether this executor may still write durable state — the two latching postures, asked in
    /// one place so a new publication kind cannot miss one.
    ///
    /// Deliberately **not** including the WAL's poison flag: that one is recoverable and is asked
    /// separately by the callers that care (`rotate_wal` cannot append at all; `plan_flush` refuses
    /// for the apply-anyway reason). These two are terminal until a restart.
    pub(super) fn may_publish(&self) -> bool {
        !self.health.overlay_diverged.load(Ordering::SeqCst)
            && !self.health.prefix_diverged.load(Ordering::SeqCst)
    }

    /// If the WAL is degraded and the degradation is one a discard can end, end it.
    ///
    /// ## Why a node must be able to leave `WalPoisoned` without a restart
    ///
    /// The causes are transient at least as often as they are terminal — a filesystem that filled
    /// and was relieved, a device that stumbled — and the previous behaviour latched the node
    /// unready for the life of the process over any of them. That is an outage the storage did not
    /// cause. Denies were never blocked by it (they are applied in memory and answered 500 whatever
    /// the posture says, and nothing gates `/control/changes` on readiness), but routing was, and a
    /// node that will not take reads again until someone notices is not fail-closed, it is just
    /// down.
    ///
    /// ## Why the recovery discards rather than repairs
    ///
    /// By the time this runs, every caller of the region above the durable boundary has been told
    /// its write is not durable. Making those bytes durable *afterwards* is fail-open in both lanes:
    /// a refused ingest reappears, and an exhausted deny window's `unsuppress` — appended like every
    /// other entry but deliberately not applied in memory — takes effect at the next replay, undoing
    /// a suppression whose operator was told it still stood. So the region is discarded, which is
    /// precisely what a restart would do with the same file
    /// (`tessera_lifecycle::wal::Wal::discard_undurable`). Nothing is retained to make it possible
    /// and both halves of a sync failure are covered: the bytes go whether or not they reached the
    /// device.
    ///
    /// **What does not recover.** A torn append. There is no repair for it here and the WAL offers
    /// none, so such a node stays `WalPoisoned` until it is restarted — which is the honest answer,
    /// since a partial `write_all` leaves neither the file's contents nor the descriptor's position
    /// known.
    ///
    /// **What it costs a healthy node: one bool read per loop iteration**, and the loop iterates
    /// only when there was work or a wake-up. Everything below the guard is unreachable while the
    /// WAL is fine.
    pub(super) fn recover_wal(&mut self) {
        if !self.wal.is_poisoned() {
            return;
        }
        if !self.wal.is_recoverable() {
            return;
        }
        // A failure here leaves the handle exactly as it was, so the next pass tries again. It is
        // deliberately silent about failing: this runs on a timer while degraded, and a log line per
        // attempt would turn one storage fault into an unbounded stream of them.
        if self.wal.discard_undurable().is_ok() {
            // **The overlay has now diverged from the durable WAL, and stays diverged.** The
            // discard did not un-apply anything (`Wal::discard_undurable` says why), so every
            // deletion and suppression applied under lifecycle §4's apply-anyway rule is in force
            // in memory with no record behind it. Publishing a flush manifest or rotating the WAL
            // from that overlay would make a 500'd, never-acked deny permanent — contradicting
            // contracts §3.1's residual, which is that a restart does *not* carry it.
            //
            // So the node keeps serving and keeps applying denies, and publishes nothing, until an
            // operator restarts it. That costs ingest visibility and is alarmed for exactly that
            // reason: it is an operator's decision rather than a silent stall. Converging by
            // re-appending was the alternative and is rejected — it produces a state no restart
            // could have produced, which is lifecycle §4's central argument.
            if !self.health.overlay_diverged.swap(true, Ordering::SeqCst) {
                tracing::error!(
                    "ALARM: this node recovered its WAL in process, so its overlay now holds \
                     dispositions no durable record backs. It keeps serving and keeps applying \
                     denies, but publishes NO flush and rotates NO WAL until restarted — ingest \
                     stops becoming visible. Restart this node."
                );
            }
        }
        self.observe_wal();
    }

    /// Block until something may be waiting, and report whether the executor should keep running.
    ///
    /// **While the WAL is degraded this wakes on a timer as well as on the doorbell**, because
    /// otherwise recovery would be reachable only by traffic: a node whose disk recovered during a
    /// quiet period would stay unready until something arrived to wake it, and `/readyz` steers
    /// traffic away from exactly that node. The poll runs only while degraded, so a healthy
    /// executor blocks indefinitely exactly as it did.
    ///
    /// Shutdown is unchanged and still leaves only from here, after both queues have been observed
    /// empty: a timeout resumes the loop, and only a disconnect ends it.
    pub(super) fn wait_for_work(&self) -> bool {
        // **Bounded by the next tick, always.** An unbounded `recv` here is what an idle node used
        // to do, and with a periodic publisher it is wrong: the tick would fire only when traffic
        // happened to wake the loop, making visibility latency a function of load rather than of
        // `flush_max_age_secs`, and leaving reclaim un-run on exactly the quiescent node
        // lifecycle §2.1 describes.
        //
        // A poisoned WAL wants a shorter wait than the tick, so the two take the smaller.
        let until_tick = std::time::Duration::from_secs(self.flush_max_age_secs)
            .saturating_sub(self.last_tick.elapsed());
        let wait = if self.wal.is_poisoned() {
            until_tick.min(WAL_RECOVERY_POLL_INTERVAL)
        } else if let Some(backoff) = self.health.failed_cycle_backoff() {
            // A cycle is open and unpublished, its request is armed, and the retry is not due
            // yet. Waiting out the floor costs one wake instead of fifty a second.
            until_tick.min(backoff)
        } else if self.health.flush_requested.load(Ordering::SeqCst)
            || outstanding(
                &self.health.flush_in_flight,
                &self.health.flush_completed_pending,
            )
            || self.coalesce_outstanding()
            || self.merge_outstanding()
            || self.health.fold_completed_pending.load(Ordering::SeqCst)
        {
            // A flush or a coalesce is executing on the pool, or its completed unit is waiting in
            // the corresponding channel, or a `POST /control/flush` is still unconsumed. A
            // request that arrived during a tick keeps its flag, and the doorbell token it rang
            // may have been spent waking the loop for the tick that did not read it.
            // The pool cannot ring the doorbell (see `flush_submit`),
            // so this poll is what bounds publication latency on an idle node — see
            // `FLUSH_COMPLETION_POLL`. Without the coalesce arm an idle node's completed coalesce
            // waits for the next *tick*, which at a 90 s period is 90 s of a pass that has already
            // done all of its IO sitting unpublished.
            until_tick.min(FLUSH_COMPLETION_POLL)
        } else if self.fold_in_flight.load(Ordering::SeqCst) {
            // A fold is running on its own thread, which — like the pool — cannot ring the
            // doorbell. Its completion has to be noticed by polling, and the *only* reason this is
            // a separate, coarser interval from the arm above is duration: a fold runs for minutes
            // to hours where a flush runs for seconds, so the 20 ms poll would spin the loop
            // hundreds of thousands of times for one publication whose latency nobody observes.
            until_tick.min(FOLD_COMPLETION_POLL)
        } else {
            until_tick
        };
        // Shutdown is unchanged and still leaves only from here, after both queues have been
        // observed empty: a timeout resumes the loop, and only a disconnect ends it.
        !matches!(
            self.queues.bell.recv_timeout(wait),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        )
    }

    /// **The deny window**: gather the queued denies into one committable unit and commit it.
    /// Returns whether anything was found, which is what keeps [`Executor::run`] draining before it
    /// blocks.
    ///
    /// ## Why this exists
    ///
    /// One `/control/changes` request of N denies used to cost N `append → fsync → apply → swap`
    /// cycles — measured at one fsync per item and ~300 denies/second, i.e. tens of minutes for a
    /// bulk revocation, with every other deny behind it and ingest starved throughout. The fsync is
    /// only half of it: a per-item path clones the whole [`Overlay`] each time, and the overlay
    /// shrinks only at a fold — so an N-item revocation
    /// also copies Θ(N²) entries. A window pays both once.
    ///
    /// It takes **two** halves to get that, and neither works alone. This is the executor half; the
    /// other is that `/control/changes` enqueues its whole request before collecting any receipt
    /// ([`LifecycleHandle::enqueue`]). With a caller that waits between items the queue never holds
    /// more than one job per requesting thread, and this function gathers exactly one entry.
    ///
    /// ## The close policy
    ///
    /// **The queue observed empty, or [`DENY_WINDOW_MAX_ENTRIES`] entries, whichever comes first.**
    /// No linger, no age bound, no timer.
    ///
    /// The bound is checked **inside** the drain and is not optional. Every entry pulled is one a
    /// concurrent submitter can replace, so "drain until the queue is empty" terminates only when
    /// the arrival rate drops — under sustained deny load from several requests it need not
    /// terminate at all, and an unbounded window is not "a big window", it is **no deny ever being
    /// acked**. That is the same failure the ingest window's row bound exists for, and the same
    /// remedy.
    ///
    /// **A linger — holding the window open to gather company — is declined.** Its whole benefit is
    /// gathering more denies, and after the enqueue split a request's denies are *already* in the
    /// queue with nothing to wait for; what a linger would additionally gather is denies from a
    /// *different* request that happens to be milliseconds behind. The cost is paid by every
    /// single-deny revocation on an idle node, which is the case the deny lane's latency exists for.
    /// A linger is also the one mechanism here that can make a deny wait for a deny that never
    /// comes.
    ///
    /// **What that leaves, stated because it is measured rather than argued away**: the executor can
    /// wake on the first item's doorbell and commit a window of one or two while the caller is still
    /// enqueueing the rest. The caller enqueues at memory speed and the first window costs an fsync,
    /// so this is a small constant number of extra windows at the head of a request, not N of them.
    /// `a_change_batch_of_n_costs_one_fsync` asserts the bound it produces rather than assuming it
    /// is zero.
    ///
    /// ## What this does not change
    ///
    /// The lane. It is still unbounded, still drained to empty before any work, still never refused
    /// for load, and there is still no route from it to a 429. The bound above closes a window; it
    /// refuses nothing.
    pub(super) fn run_deny_pass(&mut self) -> bool {
        let mut entries: Vec<DenyEntry> = Vec::new();

        while entries.len() < DENY_WINDOW_MAX_ENTRIES {
            let Ok(job) = self.queues.deny.try_recv() else {
                break;
            };
            let Job { command, respond } = job;
            let Command::Change { entity, op } = command else {
                // Unreachable while the lane follows the command (`Command::is_never_shed`): only a
                // `Change` rides the deny queue. Executed rather than dropped, so a future variant
                // that lands here is answered instead of silently losing its waiter — and the
                // window gathered so far is committed **first**, because this arm applies
                // immediately and would otherwise be applied ahead of denies that arrived before
                // it. Append order must equal apply order (lifecycle §4).
                if !entries.is_empty() {
                    self.commit_denies(std::mem::take(&mut entries));
                }
                self.execute(Job { command, respond });
                return true;
            };
            entries.push(DenyEntry {
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op,
                },
                entity,
                op,
                respond: Some(respond),
            });
        }

        if entries.is_empty() {
            return false;
        }
        self.cascade_dependents(&mut entries);
        self.commit_denies(entries);
        true
    }

    /// Add a deletion for every artifact that depends on one this window deletes
    /// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
    /// rule 1).
    ///
    /// **Extra entries in the window, and nothing else.** A cascaded deletion is a deletion: it
    /// gets its own `ChangeByEntity` record in the same append, is applied to the same overlay
    /// clone, hides its artifact at the same ack, and retires at the compaction fold that executes
    /// it — Rule F (write-path §5.4), by the same route as the deletion that caused it. There is no
    /// second removal rule here and there must never be one; a cascade that retired anywhere else
    /// is the fail-open two removal rules have been conflated into twice already.
    ///
    /// **Before the append, so a restart agrees with the live node.** The records are in the log,
    /// so replay rebuilds the same overlay rather than re-deriving the cascade from a store whose
    /// edges a later publication may have changed.
    ///
    /// Only `Delete` cascades. A suppression is reversible and retires only on unsuppress (Rule S),
    /// so cascading one would need an inverse nothing carries — and the dependent is withheld while
    /// its target is suppressed anyway, by the serving predicate's dependency term rather than by
    /// any state.
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
                respond: None,
            });
        }
    }

    /// `append × k → one fsync → apply → one swap → ack × k`, with lifecycle §4's deny-op
    /// exception folded per entry.
    ///
    /// ## Order
    ///
    /// Append order is entries order is apply order, and entries order is the deny lane's FIFO
    /// arrival order. One vector, built once and iterated forwards, so a `suppress D` and a later
    /// `unsuppress D` in the same window resolve exactly as they would have as two separate
    /// commands. This is why denies need none of the ordering machinery a *mixed* window would
    /// (`tessera_lifecycle::window` argues why the two windows stay separate).
    ///
    /// ## A failed sync is retried before it is a failure
    ///
    /// A sync failure and an append failure are different events, and the window treats them so.
    /// Every append having landed means the window's records are exactly the log's undurable region,
    /// which is the precondition for repairing it: the executor re-writes them and syncs again, a
    /// bounded number of times ([`Executor::retry_deny_durability`]). If a re-attempt succeeds the
    /// window is durable and takes the ordinary path — apply, one swap, **200** to every waiter,
    /// because the dispositions genuinely are durable and any other answer would be a lie in the
    /// direction that costs a caller a retry it does not owe.
    ///
    /// The fold below is therefore what happens when the retries are **exhausted**, or when an
    /// *append* failed and there was never anything to repair.
    ///
    /// ## The failure fold, which is the part to get right
    ///
    /// On an unrepaired append or fsync failure anywhere in the window:
    ///
    /// - every [`ChangeOp::Delete`] and [`ChangeOp::Suppress`] **in the window** is applied anyway —
    ///   the items are hidden immediately — and every waiter still gets an error;
    /// - every [`ChangeOp::Unsuppress`] applies **nothing** — the whole non-deny class, since
    ///   decision 0048 deleted `Predicate`.
    ///
    /// The scope is lifecycle §4's and it is not uniform, which is what distinguishes this from the
    /// ingest window's failure path (`Executor::fail_window_wal` applies nothing at all). Making it
    /// uniform in *either* direction is a defect: applying everything re-exposes an item that replay
    /// still hides, behind a 500 whose body says nothing was applied; applying nothing leaves a
    /// requested suppression unapplied, which is the one thing this lane may never do.
    ///
    /// **Position in the window is not a term.** §4's rule is about the op, not about whether this
    /// particular record happened to be appended before the failure — and making visibility depend
    /// on where in an arbitrary drain order an item landed would be the less fail-closed reading of
    /// the two.
    ///
    /// **What a restart then does with the window, and the one thing it costs.** Replay reads only
    /// the log's durable prefix (`tessera_lifecycle::wal`), so every record this window appended is
    /// discarded: the `Unsuppress` that was correctly refused stays refused, and the `Suppress` that
    /// was applied in memory comes back **unhidden**. That is the honest reading of the 500 the
    /// waiters received — durability was not achieved, it is owed, and the caller must retry
    /// (contracts §3.1, lifecycle §4) — and it is what the append-failure case has always done
    /// anyway, since a `Suppress` whose *append* failed leaves no bytes to replay. The two adjacent
    /// failure points agree, which is what lets an operator reason about the answer at all: a hiding
    /// that survived a restart only when the failure happened to land on the fsync rather than on
    /// the append would be a guarantee nobody could state. The retry above is what makes this the
    /// last resort rather than the first response; it does not change what the resort is.
    ///
    /// **The in-memory rule above is untouched by that**, and must stay so. The item is hidden from
    /// the moment the disposition is accepted until the process ends, which is the whole interval a
    /// live node can be asked about, and the node stops claiming readiness for the rest of it.
    /// Nothing else may come to depend on an under-durable deny: lifecycle §4 gates side-manifest
    /// publication on WAL durability on exactly this reasoning, so that no other node can observe a
    /// suppression a restart here would drop.
    /// **⊘ Specified, not implemented** — there is no replication and no side-manifest publication,
    /// so the obligation is on whoever builds one, not a property to be relied on today.
    pub(super) fn commit_denies(&mut self, entries: Vec<DenyEntry>) {
        let mut failed_at: Option<(usize, WalError)> = None;
        for (i, entry) in entries.iter().enumerate() {
            if let Err(e) = self.wal.append(&entry.record) {
                failed_at = Some((i, e));
                break;
            }
        }
        // **One fsync for the whole window.** Every entry is durable when it returns, or none is.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                // Every append landed cleanly, so the window's records are exactly the undurable
                // region and the sync can be attempted again — see `retry_deny_durability`. Only if
                // that gives up does this become a failure: the first waiter then gets the real
                // error and the rest `Poisoned`, exactly as the ingest window does, and for the same
                // reason: no wire behaviour distinguishes them.
                if let Err(e) = self.retry_deny_durability(&entries, e) {
                    failed_at = Some((0, e));
                }
            }
        }
        self.observe_wal();

        if let Some((index, error)) = failed_at {
            // Lifecycle §4's exception, per entry — see this function's doc for why the fold is not
            // uniform and why position is not a term in it.
            let applied: Vec<(EntityId, ChangeOp)> = entries
                .iter()
                .filter(|e| matches!(e.op, ChangeOp::Delete | ChangeOp::Suppress))
                .map(|e| (e.entity, e.op))
                .collect();
            if !applied.is_empty() {
                // **Deliberately does not mark the overlay dirty.** These entries were applied
                // under the apply-anyway rule and then answered 500 — they are in force in memory
                // with no durable record behind them, and contracts §3.1's residual is that a
                // restart drops them. Publishing them would make a never-acked deny permanent on
                // every restore, which is the fail-open `overlay_diverged`'s gate also guards. The
                // gate would refuse this window anyway; not setting the flag is the primary
                // reason it never arises.
                self.apply_changes(applied);
            }
            let mut real = Some(error);
            for (i, entry) in entries.into_iter().enumerate() {
                let e = if i == index {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                if let Some(respond) = &entry.respond {
                    self.ack_failed(respond, ExecError::Wal(e));
                }
            }
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);

        let applied: Vec<(EntityId, ChangeOp)> = entries.iter().map(|e| (e.entity, e.op)).collect();

        // One overlay clone, one generation, **one swap** for every entry in the window.
        //
        // **Every window owes the disc a publication.** There used to be a test here for whether
        // any entry touched deny state, because a `Predicate` change's durable home was the WAL
        // alone and no manifest field carried one; with that op deleted (decision 0048) each of the
        // three remaining ops moves state a `SEGMENTS-<n>.json` carries, and a window is never
        // empty — `commit_denies` is only ever called with entries.
        self.apply_changes(applied);
        self.deny_dirty = true;
        self.windows_since_publication += 1;
        // The liveness floor: a drain that never closes still publishes. See
        // `OVERLAY_PUBLICATION_MAX_WINDOWS`.
        if self.windows_since_publication >= OVERLAY_PUBLICATION_MAX_WINDOWS {
            self.publish_overlay_state();
        }

        // A death partway through this loop leaves some waiters acked and
        // some not; every un-acked one gets `SubmitError::ReceiptLost` → 500, never `ExecutorDead`
        // → 503, because its change is durably in force.
        for entry in entries {
            if let Some(respond) = &entry.respond {
                self.ack(respond, Ack::Changed);
            }
        }
    }

    /// A deny window's sync failed. Re-write its records and sync again, up to
    /// [`DENY_DURABILITY_ATTEMPTS`] times in total, and report whether durability was reached.
    ///
    /// ## Why the deny lane retries and the ingest lane does not
    ///
    /// The two lanes' failure paths are not symmetric, and the asymmetry is the whole justification.
    /// An ingest window whose durability fails **applies nothing** — no effect exists anywhere, the
    /// caller is told so, and a restart agrees with the caller. Nothing diverges, so there is
    /// nothing for a retry to rescue. A deny window's failure applies its deletions and
    /// suppressions anyway (lifecycle §4), so the live node hides an item that a restart un-hides:
    /// the *only* case in the write path where reaching durability late changes what the system is,
    /// rather than only what it says. [`tessera_lifecycle::wal::Wal::retry_durability`] is
    /// lane-agnostic and the ingest window could adopt it; it has no reason to.
    ///
    /// ## Why re-writing is the retry, and why it duplicates nothing
    ///
    /// A bare second `fsync` is not a retry on Linux: after a writeback error the kernel may mark
    /// the page clean and report the error exactly once, so the second call returns success with the
    /// data gone. `Wal::retry_durability` therefore rewinds to the last durable offset and writes
    /// the window's records again, re-dirtying exactly the pages that may have been dropped — and
    /// because that region is by construction the region no caller was ever told about, the repair
    /// leaves one copy of each record rather than two. (Two copies would replay correctly as well,
    /// since a disposition is idempotent; that is the fallback argument, not the mechanism.)
    ///
    /// ## What it costs, stated because it is a real regression on one axis
    ///
    /// The apply-anyway rule fires up to ~250 ms later than it did, because the retry runs
    /// **before** the window is applied rather than after. Applying first and retrying second would
    /// keep the hiding immediate, but it would split one window's application in two — the deny ops
    /// now, the rest after the retry — and this window's ordering guarantee is that entries order is
    /// apply order, which a `suppress D` followed by an `unsuppress D` in one window depends on. The
    /// added delay is inside the range the lane already exhibits under sustained ingest (165 ms p50,
    /// 346 ms max, measured); the ordering is not negotiable.
    pub(super) fn retry_deny_durability(
        &mut self,
        entries: &[DenyEntry],
        first: WalError,
    ) -> std::result::Result<(), WalError> {
        // Cloned only on the failure path, and this is the one place the executor needs the window's
        // records as a slice. A window is at most `DENY_WINDOW_MAX_ENTRIES` small records.
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

    /// **The commit window** (lifecycle §5.1): drain the work queue into one window and
    /// close it. Returns whether anything was done, which is what tells [`Executor::run`] to
    /// re-drain the deny lane rather than block.
    ///
    /// ## The two close triggers, and why only one of them is a policy
    ///
    /// - **The row bound** (`commit_window_max_rows`) — the only one of the two that is a policy.
    ///   Checked **inside** the drain, not after it: every entry pulled frees a bounded-queue slot
    ///   that a concurrent submitter refills at once, so under sustained load the `try_recv` below
    ///   never returns `Err` and "close when the queue is empty" bounds nothing at all. Tripping it
    ///   **returns**, for the same reason read the other way round: a pass that closed and carried
    ///   on draining would not come back here — or to the deny lane — until the load stopped.
    /// - **The work queue observed empty** — *structural, not a policy*. (The **work** queue: the
    ///   deny lane is not consulted here at all.) The alternative is not a different trigger; it is
    ///   a window of un-appended, un-acked ingest surviving `bell.recv()` indefinitely.
    ///
    /// A third close is forced by an entry naming an **external id the window already holds** —
    /// see `CommitWindow::holds_external_id_of`.
    /// That one is a correctness mechanism (it is what keeps the unreachable-duplicate hole closed
    /// across a window), not a policy; it also yields, for the reason written at the site.
    ///
    /// ## Why there is no age bound, and why the config key is deleted
    ///
    /// The specified third trigger is `opened_at.elapsed() >= commit_window_max_age_ms`, whose
    /// stated purpose is to stop a lone ingest on an idle server waiting the full window age
    /// "for company that is not coming". **It is declined, and `ingest.commit_window_max_age_ms`
    /// is deleted** (docs/decisions/0034-the-window-does-not-linger.md carries the no-linger
    /// argument; docs/decisions/0045-inert-config-keys-are-deleted.md the key's removal).
    ///
    /// An age bound is the safety cap on a **linger** — "having drained the queue empty, wait for
    /// more" — and this executor has no linger. A window is a local of this function and every exit
    /// disposes of it; there is no `CommitWindow` on `Executor` and no path on which one survives
    /// `bell.recv()`. So the interval an age bound would terminate does not exist, and the only
    /// place such a check could fire is *inside* the drain, where it would be a less predictable
    /// spelling of the row bound: the loop's per-entry work is hashing, and the rows it can gather
    /// are bounded by `max_rows` above (worst case `max_rows - 1 + ingest_max_batch_rows`, ≈ 20 000
    /// at the shipped defaults) and, for HTTP submitters, by `ingest_admission` as well — an
    /// admission permit is held to the receipt, and nothing in an open window has been acked.
    ///
    /// **The join qualifies the row bound, and the qualification belongs here.** A joined retry
    /// ([`Executor::admit_ingest`]'s `Held` arm) consumes a work-queue slot and adds **zero rows**,
    /// so on a stream of nothing but byte-identical retries the row bound cannot trip and this loop
    /// terminates only on an empty queue. What still bounds it is the structural fact — one entry,
    /// or one joined waiter, per concurrently-blocked submitting thread, since every submitter
    /// blocks on its receipt. That is a bound on *waiters*, not on rows or bytes, and it costs one
    /// `Responder` each; resident rows are unaffected, because a
    /// join carries no rows into the window. Deny latency is *better* on this path than the
    /// alternative of closing per retry: one close for N retries rather than N.
    ///
    /// The interval where the queue momentarily empties while more work is imminent **is** real (a
    /// handler holds its permit across decode, term resolution and sidecar IO before it submits).
    /// But that is a window closing *too early*, and an age bound only ever closes a window
    /// *earlier* — it is the wrong sign. The mechanism that would address it is a linger, which is
    /// declined: it would be paid by every submission, could gather at most the other admitted
    /// handlers, and the sort-scope win it would buy is of order 10¹ runs against the corpus's real
    /// signature distribution (`tessera_lifecycle::window`'s module doc has the arithmetic).
    ///
    /// Lifecycle §5.1 asks for a window "bounded by size **or** age". It is bounded — by size, and
    /// by a drain-empty close that is strictly tighter than any age bound could be.
    ///
    /// ## Deny priority is unchanged
    ///
    /// The deny lane is drained to empty before this is called and again as soon as it returns, and
    /// **the window holds ingest only**. Lifecycle §5.1 permits deny dispositions to share it; the
    /// permission is declined, and the argument is at `tessera_lifecycle::window`'s module doc,
    /// where a reader considering the mixed window will meet it.
    ///
    /// So a deny waits at most for the window in front of it — but only because **every close in
    /// this function yields**. The bound is not "the deny lane is drained around this call": this
    /// function is what decides how long "around" is, and while work keeps arriving it decides that
    /// by returning at each close. `a_deny_is_never_queued_behind_work_with_group_commit_disabled`
    /// holds the row-bound path (red the moment that arm loops instead) and
    /// `a_deny_is_never_queued_behind_a_conflict_forced_window_split` holds the conflict path.
    ///
    /// **Measured, because it is easy to attribute the bound to the wrong line**: swapping the two
    /// drains in `Executor::run` so the deny lane is visited *after* the work pass rather than
    /// before leaves all three of those tests green, and is not a defect — a deny still waits at
    /// most one window either way. Draining the deny lane only once the work queue has gone *empty*
    /// reds all three. The yield is the mechanism; the drain order is not.
    ///
    /// **The honest bound, stated in full.** A deny waits for the deny entries ahead of it (that
    /// lane is FIFO and unbounded) plus **at most two window closes** — and in practice one,
    /// because the replacement a conflict opens is closed empty on every path where the first
    /// close succeeded (see the conflict arm). The worst case is two only when the first close
    /// *failed*. What those closes cost is one `assign_sorted` run over the window's
    /// rows, one append per entry, **one fsync** (~3.2 ms measured, ingest baseline memo) and one
    /// `IngestBuffer` clone that is O(total buffered items) — the dominant term, bounded by
    /// `ingest_buffer_max_items` now that flush drains it (see [`Executor::apply_window`]). That is why
    /// this is a **starvation** bound and deliberately not a latency target: the window in front may
    /// be arbitrarily slow, and nothing here is sized to make it fast.
    ///
    /// A bound of "≈ 2 × `commit_window_max_age_ms`" is **not** what this code gives, and its two
    /// premises — an age bound, and denies joining the ingest window — are both false of it.
    pub(super) fn run_work_pass(&mut self) -> bool {
        let max_rows = self.health.commit_window_max_rows();
        let mut window: CommitWindow<Responder> = CommitWindow::new(self.next_window_seq());
        let mut did_work = false;

        loop {
            if window.rows() >= max_rows {
                // **Return rather than keep draining.** The drain frees a bounded-queue slot per
                // entry and a concurrent submitter refills it at once, so `try_recv` below never
                // returns `Err` under sustained load: a pass that closed a window and carried on
                // draining would never yield to `Executor::run`'s deny drain for as long as ingest
                // kept arriving, and the deny lane's bound would be the *load*, not the window in
                // front of it (lifecycle §1.3's prohibition is on a deny queued behind work of
                // unbounded duration). Returning costs one `try_recv` per window and restores the
                // bound this function's doc claims.
                // `a_deny_is_never_queued_behind_work_with_group_commit_disabled` is red on
                // `continue` here and green on `return`.
                self.close_window(window);
                return true;
            }
            let Ok(work) = self.queues.work.try_recv() else {
                break;
            };
            let job = match work {
                ExecutorWork::Lifecycle(job) => job,
                ExecutorWork::PublishGeometry {
                    publication,
                    respond,
                } => {
                    // **The open window closes first, and that is ordering rather than tidiness.**
                    // A publication swaps the whole generation; performing it while a window holds
                    // ingest that has not been applied would publish geometry against a buffer the
                    // window is about to replace, and the window's own swap would then carry the
                    // pre-publication bundle forward — losing the publication entirely. The same
                    // hazard the `Change`-shaped arm below is warned about, reached by the one
                    // variant that does make it here.
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
                    // The open window closes first, on the arm above's reasoning exactly: this
                    // swaps the whole generation, and doing it under a window that has not applied
                    // its ingest would have the window's own swap carry the pre-drop indexes
                    // forward — losing the drop, and leaving the test asserting against a state it
                    // asked to leave.
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
                    // The open window closes first, on the arm above's reasoning: the rebuild reads
                    // the live minter, and a window holding an ingest that mints has not published
                    // its value yet — so a rebuild taken under it would omit exactly the value the
                    // caller asked for the rebuild to pick up.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.rebuild_suggestion_index_now(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
            };
            let Job { command, respond } = job;
            let Command::Ingest {
                rows,
                batch_id,
                body_hash,
                artifacts,
            } = command
            else {
                // **Every command but `Ingest` and `Change` arrives here**, which is the layer
                // registrations, the publications and the growths: the lane follows the command
                // (`Command::is_never_shed`) and only a `Change` takes the deny queue. A `Change`
                // itself is therefore unreachable, and is executed rather than dropped so that a
                // future variant is answered instead of silently losing its waiter.
                //
                // **This arm applies immediately, while a window holding earlier-arriving ingest is
                // still open**, so WAL append order stops equalling submission order. That is
                // tolerable for the four variants that reach it — each appends, fsyncs and applies
                // its own record, none touches the buffer or swaps the generation, and neither
                // registry nor artifact state depends on ingest that has not been allocated. It is
                // **not** tolerable for a deny-shaped variant, whose out-of-order apply is what
                // lifecycle §4 is written against: such a variant must close the window first.
                //
                // One consequence is load-bearing elsewhere: a publication executing here **claims
                // ordinals from a level's cursor while a window is open**, which is why an ingest
                // batch's minted key claims its own at the *close* and not at admission
                // (`Executor::mint_records`).
                self.execute(Job { command, respond });
                // This job was counted at submission on the work lane and `execute` counts nothing,
                // so it is counted here or `work_depth` drifts up one per occurrence forever — the
                // drift `note_work_refused` exists to prevent.
                self.health.note_work_refused();
                did_work = true;
                continue;
            };

            let admitted;
            let m = StageMark::now();
            (window, admitted) =
                self.admit_ingest(window, rows, batch_id, body_hash, artifacts, respond);
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
    /// **One function so that the ordering is not a statement order two edits apart.**
    /// `CommitWindow::new` stamps `opened_at`, and the close it
    /// would otherwise be stamped ahead of is the *previous* window's append, fsync, apply, swap and
    /// acks. Stamped first, a replacement charges its predecessor's whole service to itself —
    /// `record_window_service` doubles, and with it the `retry_after_s` a shed client is told. The
    /// two lines below must stay in this order, and this doc is the only warning a future editor
    /// gets, because **getting it wrong has no observable consequence**. The enumeration, which is
    /// the whole of the argument:
    ///
    /// 1. This is the only construction site of a replacement window, and its only caller is the
    ///    external-id conflict arm of [`Executor::admit_ingest`].
    /// 2. That arm returns [`Admission::YieldedAfterClose`] and the drain loop **breaks in the same
    ///    iteration**, so no *later* entry can ever enter a replacement.
    ///    The only candidate is the conflicting entry itself.
    /// 3. And that entry is refused: `apply_window` inserts its predecessor's external ids into
    ///    `established` before this function returns, so `established_collisions` sees them.
    ///
    /// **The second caller is the geometry-publication arm of [`Executor::run_work_pass`]**, which
    /// closes the open window before swapping the generation. It cannot mis-stamp: it does not
    /// admit an entry into the replacement at all, and the very next `try_recv` decides what does.
    ///
    /// A held `batch_id` is **not** a route into this function — it joins or 409s in place — so the
    /// only other ways in are two exceptions, both of which leave the ordering unobservable anyway:
    /// a close that **failed** (its `fail_window_wal`/`fail_window_alloc` paths record no accepted
    /// batch and establish nothing, so the entry *is* admitted into the replacement — but both
    /// return before the `AfterFsync` pause point and `ack_failed` carries no pause point, so
    /// nothing can park inside the mis-stamped interval to measure it, and the node's WAL is
    /// poisoned by then); and a 64-bit `digest` collision on an external id, which is not
    /// constructible.
    ///
    /// **So there is no test here**, and that is recorded rather than left as a gap someone assumes
    /// is covered: the join makes a replacement window hold *fewer* entries, not more, so there is
    /// no construction that observes the mis-stamp.
    pub(super) fn close_and_reopen(&mut self, window: CommitWindow<Responder>) -> CommitWindow<Responder> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    /// The prefix directory to write into, **derived from the generation the caller is publishing
    /// against** rather than remembered.
    ///
    /// This is compaction §4's fourth gap, closed by construction. Every write inside a bundle
    /// belongs to one prefix, and which prefix that is changes when a fold flips `CURRENT`. The
    /// alternative — a stored `PathBuf` rotated at the flip — has to be got right at all eight
    /// sites that use it, and the one that would be missed is not the flush path anybody would
    /// think to check: it is [`Executor::publish_deny_state`], where the first deny published
    /// after a flip writes its side-manifest into the prefix reclamation is about to delete. Acked
    /// deny state, absent from the restore path, no error anywhere. A derived value cannot be
    /// missed.
    ///
    /// Every caller already holds the generation it is acting on — publications load it to rebase
    /// against, and the maintenance planners load it to plan from — so this costs one `join` and
    /// no lookup.
    pub(super) fn prefix_dir(&self, generation: &Generation) -> PathBuf {
        self.bundle_root.join(&generation.prefix)
    }

    /// Take the next side-manifest number: this executor's counter, raised over every
    /// `SEGMENTS-<n>.json` present under the bundle root. See [`Executor::next_manifest_n`] for why
    /// the counter alone is not enough, and `tessera_store::highest_side_manifest_n` for what the
    /// scan covers.
    ///
    /// One publication, one scan. A caller allocating several numbers at once — the overlay
    /// publication, which takes one per partition — raises the floor itself and then takes each
    /// number from [`Executor::take_manifest_n`], so the scan does not run once per partition.
    pub(super) fn allocate_manifest_n(&mut self) -> tessera_store::Result<u64> {
        self.raise_manifest_floor()?;
        Ok(self.take_manifest_n())
    }

    /// Raise the counter over every `SEGMENTS-<n>.json` on disc, and alarm if it moved.
    ///
    /// The scan is a `readdir` per prefix and per partition directory, paid once per publication —
    /// publications are seconds apart, and the alternative is a number that may already be a file.
    ///
    /// **A floor above the counter is positive evidence of a second writer.** In single-writer
    /// operation the highest number on disc is the last one this executor took, so the two are
    /// equal at every allocation. Raising the floor keeps this node publishing rather than
    /// colliding at every number it re-plans at, which makes the state survivable and not safe:
    /// the other writer is publishing complete current state over the manifests this one rebases
    /// on. The bundle lock refuses that writer at its start (§1.2), so what is left to reach here
    /// is a file placed by hand.
    ///
    /// A bundle root that cannot be listed fails the allocation, and so the publication: an
    /// allocator that cannot see which files are present cannot say a number is free. The caller
    /// discards, its files are orphans, and the next tick re-plans — the posture every other
    /// publication failure on this path takes.
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
                 a bundle root (write-path §1.2); publications continue above it, and what the \
                 other writer has published is not reconciled with what this node holds"
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

    /// Commits one partition's side-manifest. Every publication writes its manifest through
    /// here, so two things are done here once: the manifest's ordered scalars are checked against
    /// `live_manifest` ([`crate::geometry::check_manifest_publishable`]), because a manifest is
    /// assembled by editing a clone that may be stale; and the level versions and derived files
    /// are stamped ([`artifact_coordinates`]). A refusal writes nothing.
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

    /// **The batch-id state machine, evaluated on the executor** (contracts §3.4 and its
    /// Appendix R r8, lifecycle §5.1's "idempotency across a held window").
    ///
    /// Takes the open window by value and hands it back, possibly replaced. **By value
    /// deliberately**: a `&mut` signature would force a `mem::replace` on the conflict path, which
    /// constructs the replacement *before* the close it replaces — the exact mis-stamp
    /// [`Executor::close_and_reopen`] exists to prevent.
    ///
    /// Lookup order is **durable index → open window → unknown**, and the whole reason it runs here
    /// rather than in the handler is that the two are not the same question at two different times:
    /// between a handler check and the enqueue the window can close, so a retry that saw *unknown*
    /// and then enqueued into a fresh window would have double-allocated. `control.rs` keeps its
    /// pre-submit check as the early, well-messaged path; it is advisory and this is the decision.
    ///
    /// | State | What happens |
    /// |---|---|
    /// | [`BatchState::Unknown`] | the ordinary path: external-id conflict check against the window, then `established_collisions`, then a new entry |
    /// | [`BatchState::Accepted`], same bytes | the recorded ids are replayed — re-deriving them is *impossible* for a row that supplied no external id |
    /// | [`BatchState::Accepted`], different bytes | `409`, no effect |
    /// | [`BatchState::Held`], same bytes | **join**: the caller's responder is appended to the held entry, and both receive the same ids off one allocation |
    /// | [`BatchState::Held`], different bytes | `409` **to the retry only** — see the arm |
    pub(super) fn admit_ingest(
        &mut self,
        mut window: CommitWindow<Responder>,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        respond: Responder,
    ) -> (CommitWindow<Responder>, Admission) {
        match BatchState::of(&self.live, &window, &batch_id) {
            BatchState::Accepted {
                body_hash: prev_hash,
                entity_ids,
            } => {
                if prev_hash == body_hash {
                    // **A replay mints nothing, and the zero says so**: the artifacts this batch's
                    // keys created were created when it was first accepted, and this submission
                    // created none.
                    self.ack(
                        &respond,
                        Ack::Ingested {
                            entity_ids,
                            minted: 0,
                        });
                } else {
                    self.ack_failed(&respond, ExecError::BatchConflict { batch_id });
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
                    let joined = window.join(&batch_id, respond);
                    debug_assert!(joined, "`held` just answered for this batch id");
                } else {
                    // **The 409 reaches the retry and NOT the held original, and this is the one
                    // site that decides it.**
                    //
                    // The reading taken. Contracts §3.4's "the batch has no effect" is attached to
                    // the *duplicate-external-id* 409 and means the refused submission, not some
                    // other batch; Appendix R r8 and lifecycle §5.1 then describe the held state
                    // and say only that a retry must **join** rather than treat the batch as new —
                    // neither gives a retry the power to cancel an accepted batch. And the
                    // consequences run one way: the original was accepted, its waiters are blocked
                    // on the acknowledgement it is owed, and discarding it because a *different*
                    // submission arrived with different bytes breaks the durability promise for a
                    // caller who did nothing wrong — while handing any client that can guess a
                    // batch id a cancellation primitive for someone else's in-flight write.
                    //
                    // **If the opposite reading is ever ruled**, the change is here and in
                    // `tessera-lifecycle`: mark the held entry discarded (a `bool` on `WindowEntry`,
                    // skipped by `CommitWindow::allocate` and by `held`) and fail its waiters. Not
                    // an entry *removal* — `by_batch` stores indices into `entries` and the
                    // external-id set has no refcounts, so removing one entry means repairing both.
                    self.ack_failed(&respond, ExecError::BatchConflict { batch_id });
                }
                // Either way this job occupied a work-queue slot and was counted at submission,
                // while `record_window_service` counts one completion per *entry* and a join adds
                // no entry. Without this, `work_depth` drifts up by one per retry forever and every
                // 429's `retry_after_s` inherits the drift.
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Unknown => {
                let mut admission = Admission::Admitted;
                // A conflicting entry closes the window **first**, and is then evaluated against
                // the state that close just published — which is what makes the checks below give
                // the per-command answers. **Only external ids reach
                // here**: a held batch id was answered above, without closing anything.
                if window.holds_external_id_of(&rows) {
                    window = self.close_and_reopen(window);
                    // **And yield once this entry is handled.** This close is a full
                    // `append → fsync → apply → swap → ack` inside the drain loop, and
                    // `window.rows()` resets with the replacement — so the row bound can never trip
                    // on a conflict-heavy stream. Without the yield, a pass could close unboundedly
                    // many windows without ever returning to `Executor::run`'s deny drain, which is
                    // what lifecycle §1.3 forbids verbatim: a deny queued behind work of unbounded
                    // duration. Reachable at the shipped defaults from a client re-ingesting an
                    // `external_id` that a still-open window already holds.
                    //
                    // The entry is handled first rather than yielding here, because it has already
                    // been taken off the queue and its waiter must be answered.
                    // `a_deny_is_never_queued_behind_a_conflict_forced_window_split` is the leg
                    // that holds it, and it drives this path — **not** the batch-id one, which
                    // joins rather than closing.
                    admission = Admission::YieldedAfterClose;
                }
                if let Some(entry) = self.admit(rows, batch_id, body_hash, artifacts, respond) {
                    if window.is_empty() {
                        // The in-flight gauge is armed at the **first entry**, never at window
                        // construction: an empty window is never closed, so a gauge armed there
                        // would never be cleared and `service_nanos_for_estimate` would grow
                        // without bound on an idle node.
                        self.health.mark_work_started(window.opened_at());
                    }
                    window.push(entry);
                }
                (window, admission)
            }
        }
    }

    /// The external-id admission check, on the one thread that also performs the inserts. `None`
    /// means the caller has already been answered.
    ///
    /// The batch-id half is [`Executor::admit_ingest`]'s, because it has three states and this has
    /// one.
    ///
    /// This check reads state written at **apply**, which is why an entry naming an external id the
    /// *open window* holds must close it before reaching here (see the caller).
    ///
    /// ## The membership column's keys resolve here too, and a bad one refuses this batch alone
    ///
    /// A batch naming an artifact that does not exist **on a closed layer** is refused naming the
    /// key, and nothing it carried is admitted — the standard `/control/ingest` refusals are held
    /// to, and the standard one for a growth (`artifacts-from-points.md` §6.1: the whole batch or
    /// none of it). It happens **here** rather than at the close because a window holds several
    /// callers' batches: one caller's typo may not refuse another caller's rows, and after the
    /// allocation there is no per-entry refusal left to make.
    ///
    /// **On an open layer the same key mints**, and the ordinal it will hold is *not* claimed here:
    /// see `Executor::mint_records` for why the claim belongs at the close. What is decided here is
    /// everything about that key which can still refuse one batch on its own — whether the layer's
    /// declaration admits an artifact carrying nothing but a name, and whether the batch's own
    /// column named one child under two parents.
    ///
    /// An ordinal a key already resolves to is carried from here rather than re-derived at the
    /// close — see [`tessera_lifecycle::ResolvedMembership`] for why that is safe and what it means
    /// when the artifact has gone by then.
    pub(super) fn admit(
        &mut self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        respond: Responder,
    ) -> Option<WindowEntry<Responder>> {
        // The fail-closed backstop for the widened check-to-apply race — see
        // `LiveState::established_collisions`. The overlay read here is the same generation the
        // apply below will clone from, on the same thread, so the deleted-holder exemption cannot
        // race its own delete.
        let generation = self.generation.load();
        let mut rows = rows;
        let collisions = self.live.established_collisions(
            &mut rows,
            |e| generation.overlay.is_deleted(e),
            |entity, view| {
                // The same predicate the handler answered with, read from the generation this
                // apply will clone from: the view's permutation, and the buffer beside it for the
                // rows an earlier window accepted and no flush has taken yet.
                generation.bundle.partitions.values().any(|partition| {
                    partition
                        .views
                        .get(view)
                        .is_some_and(|data| data.row_space.row_of(entity).is_some())
                }) || generation.buffer.contains_in_view(entity, view)
            },
        );
        // **The join rule's arms, on the one thread that settles join-ness** (`views.md` §4, §5;
        // decision 0116). They used to run in `/control/ingest`'s handler, a whole queue drain
        // before `established_collisions` above decided which rows are joins — so a row whose
        // holder was established in between was admitted as a join having passed no arm at all.
        // One authoritative site, and the refusal text is the handler's own so the bodies are
        // byte-identical to what the earlier site answered. `settle_joins` also completes an
        // accepted join — dropping its descriptors and terms, backfilling its omitted `render`
        // values — because that is the same per-row pass over the same sources.
        if collisions == 0 {
            if let Err(detail) = settle_joins(&generation, &mut rows) {
                drop(generation);
                self.ack_failed(&respond, ExecError::JoinRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        }
        drop(generation);
        if collisions > 0 {
            self.ack_failed(
                &respond,
                ExecError::DuplicateExternalId { count: collisions },
            );
            self.health.note_work_refused();
            return None;
        }

        let (memberships, edges) = match self.resolve_memberships(&artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                self.ack_failed(&respond, ExecError::LayerRefused { detail });
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
            waiters: vec![respond],
        })
    }

    /// Resolve one batch's membership keys, and check the edges its adjacency declared.
    ///
    /// Returns the memberships — each carrying the ordinal it resolved to, or `None` where an open
    /// layer will mint it at the close — and the edges whose **child** is one of those mints, which
    /// are the only edges this route creates rather than checks.
    ///
    /// `Err` is the refusal text the caller is answered with, whole batch without effect.
    ///
    /// **The memberships resolve first, and that order is what the edge checks rest on**: a key is
    /// created only by being a membership (every entry of a list column names an artifact the point
    /// belongs to — `artifacts-from-points.md` §4), so the set of keys this batch is about to mint
    /// is known once they are done, and neither a child nor a parent can be minted without
    /// appearing there.
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
        // **One line per batch, not one per edge.** A lineage over a 10⁵-cluster tree whose roster
        // was published without parents would otherwise emit 10⁵ formatted writes on the write
        // path, which is the shape that makes a log a second bottleneck. The count is the signal
        // and the examples are what an operator acts on.
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
            // **Two indexes of one set, because an edge asks two different questions of it.** A
            // *child*'s level is the edge's own, so it is asked precisely; a *parent*'s is whatever
            // the layer's shape says to look at, so it is asked of the layer. A levelled taxonomy
            // legitimately carries one key at two levels, and one index would treat a key minting
            // at one of them as minting at both.
            let minting: std::collections::BTreeSet<(&str, u32, &str)> = memberships
                .iter()
                .filter(|m| m.ordinal.is_none())
                .map(|m| (m.layer.as_str(), m.level, m.key.as_str()))
                .collect();
            let anywhere: std::collections::BTreeSet<(&str, &str)> = minting
                .iter()
                .map(|(layer, _, key)| (*layer, *key))
                .collect();

            // **A child named under two parents refuses the batch**, over the batch's own column,
            // because that is where the two rows are — and it is the whole check for a minted
            // child, whose parent nothing else has an opinion about yet. Only a `nested` or
            // `tiered` list column declares edges; a `dag` layer's several parents arrive on its
            // artifact rows' `parent` list by the publish route, never here.
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
                    // The child does not exist yet, so this edge is its parent rather than a claim
                    // about a stored one — carried to the close, where the artifact is created and
                    // where lineage has always been settled.
                    Ok(tessera_lifecycle::EdgeCheck::Mints) => mints.push(edge.clone()),
                    // **Reported, not refused** — see `LayerRegistry::check_edge`. The membership
                    // half of the same entry is unambiguous and lands; what is lost is an edge this
                    // route cannot create, and an operator who published a roster without its
                    // parents needs to be told rather than blocked.
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
                 memberships are applied and the edges are not — a growth adds members, and \
                 lineage is declared where the artifact is published"
            );
        }
        resolved
    }

    /// **The artifacts this window's *values* named and nothing holds** — one per
    /// `membership = { attribute = f }` layer whose column carried a value the level has no
    /// artifact for.
    ///
    /// **A value exists because a point carries it**, at both entry points: a build mints from the
    /// column it has just read, and an ingest mints from the rows that have just arrived. The two
    /// use the same rule for the key (`tessera_types::layer::attribute_value_key`), which is what
    /// makes them agree about which artifact a value names — a key that could be written two ways
    /// would let one route mint a second artifact for a value the other already named.
    ///
    /// **After the vocabulary mint, and that ordering is load-bearing.** A novel category key is a
    /// string in the row until the pass above draws it a code; reading the row before that would
    /// name the artifact after a code nobody had assigned yet.
    ///
    /// **Suppression-blindness carries over unchanged.** The lookup is
    /// [`ArtifactStore::ordinal_of_key`] — the store's key index, which loses a key at exactly one
    /// event, the fold retiring the artifact's own entity. A *suppressed* value's key therefore
    /// still resolves and mints nothing, so a suppression cannot be defeated by ingesting a point
    /// carrying the value; a *deleted* one does mint again, and the new artifact is a new object
    /// with a new entity, which is what a deletion means.
    ///
    /// **Publication into such a layer stays refused** — this is not that route. What is created
    /// here is an identity the rule produces, carrying its key and nothing else, on
    /// `LayerRegistry::prepare_derive`'s own contract.
    pub(super) fn derive_records(
        &mut self,
        closed: &[tessera_lifecycle::ClosedEntry<Responder>],
        vocabularies: &Vocabularies,
    ) -> Result<Vec<WalRecord>, String> {
        use tessera_types::layer::attribute_value_key;

        // Which declared scalar each predicate layer reads, resolved once. A layer naming a column
        // this bundle does not declare is refused at registration, so an absence here is a
        // declaration that never validated — skipped rather than guessed at, which mints nothing.
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
            // `code → key`, for the values this window actually carried. Walked from the live
            // bindings rather than inverted per row: a vocabulary is a map from key to code, so a
            // per-row reverse lookup would rebuild this per point.
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
                    // A predicate layer is entity-scoped: `LayerRegistry::prepare_derive`
                    // refuses a group-scoped one, so the key sits in the one set.
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
    /// holds — see [`mint_plan`] for what is minted and why it is minted here.
    ///
    /// Patches the memberships whose key resolved *since* admission to the ordinal it resolved to,
    /// so they grow rather than mint; leaves a minted key's ordinal `None`, which is what tells
    /// [`growth_records`] the publication carried the join.
    ///
    /// `Err` is the refusal text every waiter in the window is answered with. It costs the window,
    /// which is the price of a decision that can only be made once the batches are together — and
    /// the two shapes that reach it are a lineage two *batches* disagree about, and an allocator
    /// that could not supply the reserved run. Everything a single batch can be refused for on its
    /// own was refused at its admission.
    pub(super) fn mint_records(
        &mut self,
        closed: &mut [tessera_lifecycle::ClosedEntry<Responder>],
    ) -> Result<(Vec<WalRecord>, Vec<u64>), String> {
        let mut minted_per_entry = vec![0u64; closed.len()];
        let Some((wanted, edges)) = mint_plan(closed) else {
            return Ok((Vec::new(), minted_per_entry));
        };
        let (records, resolved, minted) = self.prepare_mints(&wanted, &edges)?;

        // A key that acquired an artifact between its batch's admission and this close is an
        // ordinary growth, and `growth_records` takes it from there — see
        // [`settle_resolved_ordinals`], which the values door settles by too.
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

    /// **Prepare one set of mints** — the publications that create the artifacts a caller's keys
    /// named and no artifact holds.
    ///
    /// **One implementation across the doors** (decision 0139): `/control/ingest` reaches it
    /// through [`Executor::mint_records`] at its window's close, and `POST /control/values`
    /// through [`values_mint_plan`] at its own commit, so a key arriving at either door creates
    /// the same artifact, with the same lineage and the same refusals. Nothing here reads which
    /// door it was called from.
    ///
    /// The three answers: the records to append **in the order given** — ascending level, coarse
    /// first, so a tiered chain's parent is fixed by the record before its child's — the keys
    /// that turned out to be held after all, which their caller grows into instead, and the keys
    /// this run minted.
    pub(super) fn prepare_mints(
        &self,
        wanted: &MintPlan,
        edges: &[tessera_lifecycle::BatchEdge],
    ) -> PreparedMints {
        use std::collections::BTreeMap;
        self.live.with_publication_state(|registry, store, alloc| {
            // **A child named under two parents refuses**, across the window as it does within a
            // batch. Checked before anything is prepared, so a refusal spends nothing. The cycle
            // those edges could close is refused where the artifacts are created, in
            // `prepare_publish`, which walks the batch's own edges — a growth never adds lineage,
            // so the window's minted edges are every edge a cycle could run through.
            let parents = parent_of_each_child(edges)?;
            // Re-resolved here and not trusted from admission: a publication executes between an
            // admission and this close (it takes the work lane, and the window is open across it),
            // so a key that named nothing then may name an artifact now — and §5's second ruling is
            // that a key a live artifact holds is never minted again.
            let mut resolved: BTreeMap<(String, u32, String), u32> = BTreeMap::new();
            let mut to_mint: BTreeMap<(&str, u32), Vec<(&str, &croaring::Bitmap)>> =
                BTreeMap::new();
            for ((layer, level, key), (_, members)) in wanted {
                // The ingest route carries no artifact view; `resolve_or_mint` refuses a
                // group-scoped layer there (`ingest.md` §1.5), so the key sits in the one set.
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

            // **Ascending level, one record each, coarse first.** A tiered chain's parent sits one
            // level up and is fixed by the record before this one; a nested lineage is level 0
            // alone, one record, and `prepare_publish` resolves a parent that is a sibling of its
            // own batch.
            let mut assigned: BTreeMap<(&str, u32, &str), u32> = BTreeMap::new();
            let mut records = Vec::new();
            for ((layer, level), keys) in &to_mint {
                let incoming: Vec<tessera_lifecycle::IncomingArtifact> = keys
                    .iter()
                    .map(|(key, members)| tessera_lifecycle::IncomingArtifact {
                        key: Some((*key).to_string()),
                        // Entity-scoped: a group-scoped layer is refused at admission, a point's
                        // layer column carrying no artifact view (`ingest.md` §1.5).
                        view: None,
                        members: (*members).clone(),
                        excluding: None,
                        // **Nothing but its name.** A layer declaring supplied content or a
                        // dependency refuses the key at admission rather than minting an artifact
                        // that could not be served — `LayerRegistry::resolve_or_mint` makes both
                        // refusals, in the words `prepare_publish` would have made them in.
                        contents: Vec::new(),
                        attached_to: None,
                        parent_keys: parents
                            .get(&(*layer, *level, *key))
                            .map(|parent| vec![(*parent).to_string()])
                            .unwrap_or_default(),
                        // A layer declaring a `shape` publishes boxes an author wrote, so a point
                        // naming a key on such a layer has nothing to mint one from — the layer is
                        // a predicate and `resolve_or_mint` refuses the key at admission.
                        shape: None,
                    })
                    .collect();
                // **One level up and no further.** Entry *k* of a list is the parent of entry
                // *k+1*, so a chain minted from one names its parent exactly one level coarser;
                // searching the levels above that would invent an edge across a gap the reader
                // deliberately does not read past (`tessera_types::layer::parent_edges`).
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
                // Read back off the record rather than recomputed from the level's cursor: what a
                // finer level's parent resolves to is what this record actually claimed. The order
                // is `incoming`'s, which is `keys`', which is why the two zip.
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

    /// **Close a commit window**: one signature-sorted allocation run, one WAL record per entry, one
    /// fsync, one generation swap, then every waiter is acked.
    ///
    /// ## I9 is untouched
    ///
    /// "The window allocates" reads like an allocator change and is not one (lifecycle §5.1). IDs
    /// are still issued monotonically from the high-water by one `Allocator::allocate`, still never
    /// reused, still ordered by `(signature, external_id)` through the unchanged `assign_sorted`.
    /// What widens is the **input set**: design §11.1's sort scope becomes the window, at the
    /// server, instead of whatever chunk a client happened to POST.
    ///
    /// **A failed window burns entity ids**, exactly as a failed batch did: assignment precedes the
    /// append, so ids given to a window whose append then fails are never issued again. I9-safe —
    /// ids stay strictly monotone and each is issued once.
    ///
    /// ## The failure rule
    ///
    /// An append or fsync failure **applies nothing**, in deliberate contrast to the deny path:
    /// applying un-fsynced ingest would make items appear and vanish across a crash, and lifecycle
    /// §4's apply-anyway rule is written for `Delete`/`Suppress` only. The window carries ingest
    /// alone, so this rule is uniform over every entry in it and there is no per-entry split by
    /// disposition to get wrong. That is one of the reasons deny dispositions stay out of the
    /// window — see `tessera_lifecycle::window`'s module doc for the rest.
    /// `an_ingest_append_failure_applies_nothing` holds this rule and
    /// `an_unsuppress_append_failure_applies_nothing` holds the deny lane's op scope beside it.
    ///
    /// **A restart does not undo the refusal.** Replay reads only the log's durable prefix
    /// (`tessera_lifecycle::wal`), so the window's records — which by construction lie past the last
    /// fsync — are discarded rather than replayed. Without that, an ingest refused for want of
    /// durability would exist after the next restart: the caller was told it had nothing, so a
    /// caller doing what the 500 asks and retrying under a fresh batch identifier would end up
    /// holding two. `an_ingest_whose_durability_failed_stays_absent_across_a_reopen` holds this, and
    /// `crash_between_fsync_and_swap_replays_rather_than_reallocates` holds the other half — a real
    /// killed process whose window *did* fsync, which replays rather than reallocating.
    pub(super) fn close_window(&mut self, window: CommitWindow<Responder>) {
        let entries = window.len() as u64;
        let started = window.opened_at();
        let mut mark = StageMark::now();

        let closed = match self.live.with_allocator(|a| window.allocate(a)) {
            Ok((closed, tally)) => {
                // Recorded here, at the allocation, rather than after the append: the figure
                // describes the ASSIGNMENT, which is made and complete by this point. A window that
                // allocates and then fails its append has still fragmented the entity axis exactly
                // this much, because the ids are issued and `Allocator` never reuses one (I9).
                self.health.record_fragmentation(tally);
                closed
            }
            Err((e, waiters)) => {
                // `allocate` leaves the high-water mark unchanged on this path, so the window has no
                // effect at all — the same statement `ExecError::Alloc` already makes per batch.
                self.fail_window_alloc(e, waiters, entries, started);
                return;
            }
        };

        mark = self.health.lap(WriteStage::Allocate, mark);

        // **Mint every novel discovered-vocabulary key this window's rows carry, in place, before
        // anything is appended.** A discovered vocabulary's key travels as `WalScalar::Utf8` from
        // the ingest boundary (`tessera-server`'s `category_code`, which must not mint itself: two
        // requests racing one novel key would each draw and split the key across two codes). The
        // commit-window close is where minting *may* happen — the live view is authoritative and
        // serial here, exactly as `VocabularyMinter::mint`'s own doc requires — so it happens once,
        // against a mutable copy of the published bindings that becomes the next generation's if
        // the window survives, and is discarded untouched if it does not.
        //
        // One `Vocabularies` copy for the whole window, not one per row: `mint` is view-first, so a
        // second row naming an already-minted-this-window key sees the first row's binding and
        // returns `Existing` rather than drawing again — which is what keeps two rows sharing one
        // novel key inside a window down to one `VocabularyMint` record.
        let generation = self.generation.load_full();
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        let declared_scalars = generation.bundle.manifest.declared_scalars.clone();
        // **The arity is this generation's, and a row admitted under an earlier one is padded
        // here** (`ingest.md` §7.1). A column declared between a batch's admission and this close
        // appended at the tail of `declared_scalars`, so the row's own positions keep their
        // meaning and the positions it lacks are columns it holds nothing for. Padded before the
        // mint pass below indexes `row.scalars` by declared position, and before the append, so
        // the log carries every row at the schema its flush will write.
        let mut closed = closed;
        for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                crate::attributes::pad_to_schema(&mut row.scalars, &declared_scalars);
            }
        }
        // **The group-scoped families, by the view a row names** (`views.md` §5). A row's scoped
        // tail is positional against the families of the group that owns its view, so the mint
        // pass below needs the same list the boundary parsed against — derived once for the
        // window rather than per row, and from the live manifest, which is what the boundary read
        // too.
        let scoped_by_view: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
            scoped_families_by_view(&generation.bundle.manifest);
        let mut fresh_bindings: Vec<(String, String, u32)> = Vec::new();
        let mut mint_failed: Option<MintError> = None;
        'minting: for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                for (index, declared) in declared_scalars.iter().enumerate() {
                    let Some(vocabulary) = declared.vocabulary.as_deref() else {
                        // A plain scalar, or a category column already at its bound width — either
                        // way, nothing for this site to resolve.
                        continue;
                    };
                    let WalScalar::Utf8(key) = &row.scalars[index] else {
                        // Already a code: either a declared vocabulary (the handler resolved it) or
                        // a discovered one this row's earlier pass through this same loop resolved.
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        // `Vocabularies::seed` refuses to open a bundle whose `declared_scalars`
                        // names a vocabulary `MANIFEST.vocabularies` does not carry, so a live
                        // generation cannot disagree with its own declaration. Reaching this is a
                        // defect in that invariant, not reachable input.
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
                // **The same mint, over the row's scoped tail** (`views.md` §5). A scoped category
                // is a category: its key travels from the boundary exactly as an entity-scoped
                // one's does, and this is the one place a novel key becomes a code. A row whose
                // view is in no scope has an empty list here and the loop does nothing.
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
            // Nothing has been appended yet, so — exactly as a failed allocation — the window has
            // no effect: the mutated `vocabularies` copy is dropped with it, and every waiter gets
            // the same refusal.
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

        // **The artifacts this window's rows named and nothing holds, created here** — see
        // `Executor::mint_records`. Prepared before anything is appended, on `prepare_publish`'s own
        // rule that every check runs before the first allocation, so a refusal spends nothing. A
        // failure past that point has spent reserved ids, exactly as a failed window's rows have.
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
        // **The artifacts this window's *values* named**, one per attribute-predicate layer whose
        // column carried a value nothing holds — see `Executor::derive_records`. It runs after the
        // vocabulary mint above, because a novel category key is a code only once that pass has
        // drawn it, and the key an artifact is named by is the value's key.
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

        // One record per entry — batch identity is preserved through the window, which is what a
        // joined retry is answered off — appended in entries order, which is also apply order.
        let mut failed_at: Option<(usize, WalError)> = None;

        // **Mint records land first, ahead of every batch record, inside the one fsync below** —
        // `WalRecord::VocabularyMint`'s own doc states this ordering is why a mint is durable in
        // the same commit as the rows it colours. Not tracked in `positions`: that vector is
        // rotation's per-*batch-entry* index, and a mint record belongs to no entry.
        for (vocabulary, key, code) in &fresh_bindings {
            if let Err(e) = self.wal.append(&WalRecord::VocabularyMint {
                vocabulary: vocabulary.clone(),
                key: key.clone(),
                code: *code,
            }) {
                // No entry has been attempted yet, so there is no "the entry whose append failed"
                // to single out — the same arbitrary choice the fsync failure below makes.
                failed_at = Some((0, e));
                break;
            }
        }

        // The position **before** each append is where that record lands, and it is the only moment
        // it can be read: afterwards the log has moved on, and after the window it is one number for
        // several records. A row's position is what a rotation reclaims below, so an entry whose
        // append failed contributes none — the loop breaks before pushing.
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

        // **The joins this window's rows declared, in the same commit as the rows** — one record
        // per `(layer, level)` over every entry, appended behind the batch records and inside the
        // one fsync below (`artifacts-from-points.md` §6.2). Built after the allocation because
        // that is the first moment a row has an entity to join with, and the ordinals were resolved
        // at admission.
        //
        // Each record's position is read before its append and carried to the apply: a growth below
        // its level's published high-water is held in the log by that position until a fold rewrites
        // the level whole, and releasing it early is the silent loss `ArtifactStore::grow`'s own doc
        // is written against.
        //
        // **The publications that minted come first**, because a growth of the same window may name
        // an ordinal one of them claimed — not today, a minted artifact being published with its
        // members, but replay applies this sequence in order and an artifact must exist before
        // anything addresses it.
        let mut minted: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for record in mint_records {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    // No entry is more to blame than another for a record the whole window's keys
                    // produced; the first waiter gets the real error, as the fsync arm does.
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
        // **One fsync for the whole window.** This is the amortisation half of group commit; the
        // allocation scope above is the point of it.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                // Every entry appended cleanly, so there is no "the entry whose append failed" here
                // — the first waiter gets the real error arbitrarily and the rest `Poisoned`. No
                // wire behaviour distinguishes them (`map_accept_error` folds `ExecError::Wal(_)`
                // variant-blind to 500); the distinction is for whoever reads the two messages.
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
        // One buffer clone, one generation, **one swap** for every entry in the window — carrying
        // the mutated `vocabularies`, so the next generation publishes this window's mints and not
        // merely its rows.
        self.apply_window(&mut closed, &positions, vocabularies, &fresh_bindings);

        // **After the rows are in force, never before.** A membership is projected through rows, so
        // a store that held the join while the generation still lacked the row would describe an
        // artifact by a point nothing could yet see. The reverse order costs nothing: both are
        // durable by this line, and the log is what a restart reads.
        let (artifact_records, artifact_positions): (Vec<&WalRecord>, Vec<u64>) = minted
            .iter()
            .chain(growth.iter())
            .map(|(record, position)| (record, *position))
            .unzip();
        self.apply_artifact_records(&artifact_records, &artifact_positions);

        // Recorded after the swap, so a concurrent replay of a batch id can never observe a window
        // where the generation has swapped but the idempotency index has not caught up.
        let m = StageMark::now();
        for (entry, wal_pos) in closed.iter().zip(&positions) {
            let (batch_id, body_hash) = entry.batch_key();
            // The index is a cache of the record just appended, so what goes in is what the record
            // says. Asserted rather than read from the record, the entry already holding both in
            // the form the ack needs; a record kind whose identity this function could not derive
            // would fail `batch_identity`'s own exhaustive match first.
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

        // **What a batch minted is reported to the batch that minted it.** Under
        // `value_set = "open"` a typo creates a permanent object rather than being refused — the
        // trade the declaration makes knowingly — and the mitigation is that it is visible: the
        // caller is told the count in its own 200, and the operator gets this line.
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

        // A death partway through this loop leaves some waiters acked and
        // some not; every un-acked one gets `SubmitError::ReceiptLost` → 500, never `ExecutorDead`
        // → 503, because its ingest is durably in force. That is the widening `ReceiptLost`'s own
        // doc predicts for this task.
        for (entry, minted) in closed.into_iter().zip(minted_per_entry) {
            let ClosedEntry {
                entity_ids,
                mut waiters,
                ..
            } = entry;
            // The last waiter takes the ids; **a joined retry is what puts a second one here**, and
            // it clones. Popping rather than an `Option` dance: `waiters` is never empty (an entry
            // is built with one), and a `Vec<EntityId>` per entry is up to `ingest_max_batch_rows`
            // long, so cloning it unconditionally would be a real per-row cost for the common case
            // of one waiter. The clone is per *retry*, not per row of the original.
            let last = waiters
                .pop()
                .expect("an entry always has at least one waiter");
            for waiter in waiters {
                let entity_ids = entity_ids.clone();
                self.ack(&waiter, Ack::Ingested { entity_ids, minted });
            }
            self.ack(&last, Ack::Ingested { entity_ids, minted });
        }

        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window could not be allocated: nothing was appended, nothing applied, and the high-water
    /// mark did not move. `AllocError` is `Copy`, so every waiter gets the real one.
    pub(super) fn fail_window_alloc(
        &self,
        error: AllocError,
        waiters: Vec<Vec<Responder>>,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in waiters {
            for waiter in entry {
                self.ack_failed(&waiter, ExecError::Alloc(error));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window's append or fsync failed: **apply nothing**, and answer every waiter.
    pub(super) fn fail_window_wal(
        &self,
        closed: Vec<ClosedEntry<Responder>>,
        index: usize,
        error: WalError,
        entries: u64,
        started: std::time::Instant,
    ) {
        let mut real = Some(error);
        for (i, entry) in closed.into_iter().enumerate() {
            for (k, waiter) in entry.waiters.into_iter().enumerate() {
                // The real error goes to the entry the failure belongs to; every other waiter gets
                // `Poisoned`, which is precisely what its own append would have returned had it been
                // attempted after the failure (`Wal::append`'s error arm poisons the handle), and
                // what the WAL will in fact return for every subsequent call.
                let e = if i == index && k == 0 {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                self.ack_failed(&waiter, ExecError::Wal(e));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// Answers every waiter of a window that was refused after allocation with the same error.
    pub(super) fn fail_window(
        &self,
        closed: Vec<ClosedEntry<Responder>>,
        error: impl Fn() -> ExecError,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in closed {
            for waiter in entry.waiters {
                self.ack_failed(&waiter, error());
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// **Hold one accepted write's delta until the tick** (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// Every route that changes a level's records — a publication, a growth, a page of a
    /// generating set, a fill — arrives here with the level version it followed, and the level's
    /// row forms take the run of them at the next tick. A record naming no level's forms is held
    /// all the same: which views hold a form is not this thread's question until it publishes.
    ///
    /// **`refused` names the growth entries the store did not take**, by their position in the
    /// record (`ArtifactStore::apply_reporting`), and they are held for nothing: a form that
    /// unioned an entity the records refused would count a member no artifact has. A record whose
    /// every entry was refused is still held, empty, because it moved the level's version and the
    /// versions of an interval's deltas must stay consecutive.
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
                // **The log's own bytes decoded, so what reaches the row forms is what reached the
                // records.** A set that does not decode is skipped and nothing else is, which is
                // the disposition `ArtifactStore::apply_growth` makes of it: that delta did not
                // enter the records either, and the alarm it raised has already been said.
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
                            // **A leave, or a withdrawal, re-derives the operator whole**
                            // (`ingest.md` §1.1): a union cannot express a leave, and the
                            // withdrawal an emptied set makes moves every rank above it. A page of
                            // joins alone is unioned into the operator that is served.
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

    /// Clone the buffer **once**, insert every entry in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows: the clone is O(total buffered items) and
    /// the buffer grows until the next tick drains it, so a window of k entries pays it once
    /// instead of k times — and the same clone is the deny-ack latency floor
    /// ([`ExecutorHealth::apply_nanos_total`]).
    ///
    /// ## What the window does NOT do to this cost
    ///
    /// It reduces the clone's **count**, not its **cost**. Two facts a reader sizing anything from
    /// the paragraph above needs, and neither is addressed here:
    ///
    /// 1. **At the shipped defaults `commit_window_max_items == ingest_max_batch_rows == 10 000`,
    ///    so a maximal batch is a one-entry window and gets no amortisation at all.** Ingesting
    ///    10⁹ rows in maximal batches is 10⁵ submissions each cloning a buffer growing towards
    ///    10⁹ — **O(N²/B)** — bounded only by the flush draining the buffer each tick and by
    ///    `ingest_buffer_max_items` when it cannot. Measured pre-flush:
    ///    `apply_nanos_max` 210–437 ms at ~1.34 M buffered items
    ///    (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`, result 3). It is the *small*
    ///    batches the window collects.
    /// 2. **Flush drains the buffer each tick**, so the clone's operand is bounded by one tick's
    ///    arrivals in the steady state and by `ingest_buffer_max_items` when flush is failing. A
    ///    chunked or persistent buffer remains the remedy if the per-window clone itself ever
    ///    measures as the constraint — do not build a second mechanism around it before that.
    ///
    /// No counter is added for this: `apply_nanos_total` / `apply_nanos_max` already
    /// measure it and are already on `/control/status`.
    ///
    /// `terms` is **taken** out of each entry rather than borrowed: each row's resolved set is
    /// *moved* into the buffer, where borrowing would force one `Vec<TermId>` clone per row on the
    /// one thread every write is serialised through — measured at +14% on the
    /// 10 000-row arm. `&mut` is what buys it; an entry's `terms` is empty after this and nothing
    /// downstream reads it — the ack needs `entity_ids`, not terms.
    ///
    /// `vocabularies` is `close_window`'s locally mutated copy — the live bindings plus this
    /// window's mints — and is published verbatim rather than `Arc::clone(&generation.vocabularies)`
    /// as every other unmoved field is: the whole reason minting happens on the executor is that the
    /// mutation must reach the *next* generation, and cloning the *old* `Arc` here would silently
    /// discard every code this window just drew.
    pub(super) fn apply_window(
        &self,
        closed: &mut [ClosedEntry<Responder>],
        positions: &[u64],
        vocabularies: Vocabularies,
        mints: &[(String, String, u32)],
    ) {
        let started = std::time::Instant::now();
        let mut mark = StageMark::now();
        let generation = self.generation.load_full();
        // The suggestion index's side map, grown by exactly the keys this window minted. Nothing
        // is rebuilt: the base index and every other vocabulary's are carried behind their `Arc`s,
        // and the fold's handles are borrows of data baked into the binary rather than a
        // deserialisation.
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
        // Entries are appended and applied in the same order, and nothing observable depends on
        // which order that is. `CommitWindow::holds_external_id_of` forces a close rather than admit
        // a second entry naming an external id the window already holds, and a row with no external
        // id establishes nothing (the `if let Some` below), so no two entries in one window can
        // write the same key. That is a stronger statement than "one vector, iterated once": it
        // survives a refactor that reorders the vector, where the shape argument does not.
        for (entry, wal_pos) in closed.iter_mut().zip(positions) {
            let terms = std::mem::take(&mut entry.terms);
            for (row, row_terms) in entry.rows().iter().zip(terms) {
                // Contracts §3.4: no external id means nothing to establish. `None` must never
                // collide with `None`, so this skips rather than inserting under a shared empty key.
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

        // Published here, and at every other place buffer occupancy changes — the flush's
        // publication and the deny lane's — so `/control/ingest`'s occupancy bound reads a figure
        // the executor maintains rather than one a handler derives from a generation it would
        // have to load.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);

        let next = generation.with(|g| {
            g.overlay_version = generation.overlay_version + 1;
            g.buffer = Arc::new(buffer);
            g.vocabularies = Arc::new(vocabularies);
            // **The one publication that changes the suggestion index**, and it changes it by the
            // same mints that changed the bindings above: a novel key gets its code here, and a
            // viewer typing its prefix on the next keystroke must be offered it rather than
            // waiting for the next rebuild (`value-suggestion.md` §6.1). Every other publication
            // carries the index forward.
            g.suggest = suggest;
        });
        self.publish(next, started);
        self.health.lap(WriteStage::ApplySwap, mark);
    }

    /// Clone the overlay **once**, apply every change in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows. [`Overlay`] never shrinks —
    /// entries survive `suppress → unsuppress` and shrink only at a fold — so the clone
    /// is O(overlay depth) and the depth rises by one per new item denied. Applying an N-item
    /// revocation one command at a time therefore copies Θ(N²) entries; a window of k pays the clone
    /// once for the k. The clone is also every deny's ack-latency floor
    /// ([`ExecutorHealth::apply_nanos_total`], already on `/control/status`, so no counter is added
    /// for this).
    ///
    /// Changes are applied in view order, which is the window's entries order, which is the deny
    /// lane's FIFO arrival order — so a `suppress` and a later `unsuppress` of the same item resolve
    /// as they would have as two separate commands. The view is iterated once, forwards.
    ///
    /// Pins are never invalidated by this (I11): a pin fixes `(prefix, segments_version)`, and this
    /// bumps `overlay_version`. That is lifecycle §2.3's rule that a suppression applies to a
    /// pinned request the moment it is accepted, without expiring the pin.
    pub(super) fn apply_changes(&self, changes: Vec<(EntityId, ChangeOp)>) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        // What the window did, for the mask below: which entities it denied, and whether any
        // removal happened at all.
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

        // **It alarms on the union; the schedule acts on the deletions.** `overlay_soft_limit`
        // gauges `deleted ∪ suppressed`, which is what an operator should see, while the fold
        // trigger it seeds keys on the retirable part — a suppression never retires, and a fold
        // dispatched on the union would rewrite the corpus to retire nothing (compaction §9).
        //
        // This is the only place the overlay grows **at runtime**; it is not the only place it
        // grows. `WritePath::reconstruct` builds one from WAL replay before this executor exists,
        // so a node restarting already over the limit is caught by
        // `Engine::set_overlay_soft_limit`'s own one-shot evaluation instead.
        // **Edge-triggered.** The depth never decreases, so a level-triggered check would emit
        // this four-line WARN on every subsequent deny, forever, with no path back — an alarm flood
        // at exactly the moment the node is under deny pressure.
        // `note_overlay_depth` returns `true` only on a crossing.
        let depth = overlay.len();
        let limit = self.health.overlay_soft_limit();
        if self.health.note_overlay_depth(depth) {
            tracing::warn!(
                overlay_depth = depth,
                overlay_soft_limit = limit,
                "ALARM: the overlay has crossed its configured soft limit. A compaction fold is \
                 the lever — it retires the executed deletions (Rule F) — but nothing schedules \
                 one: the automatic trigger and POST /control/compact are unbuilt (compaction \
                 §9), so the depth comes down only when something calls for a fold. This line is \
                 edge-triggered, so it will NOT repeat while the overlay stays over. \
                 Overlay depth is a term in I1's composition cost and in every deny's ack latency \
                 (each acceptance clones the overlay); watch overlay.depth on /control/status"
            );
        }

        // **A deleted row leaves the buffer here** — the runtime half of `replay`'s end-of-pass
        // rule, and the reason a `delete` issued before the item's first flush does not pin the
        // WAL for ever (`IngestBuffer::oldest_wal_pos` is the rotation's reclaim bound, and
        // `plan_flush` never consumes a deleted row, so nothing else would ever remove it).
        // Composition-neutral: `compose::verdict` answers from `is_deleted` before it consults the
        // buffer. The argument in full is at `tessera_lifecycle::overlay::drop_deleted`.
        //
        // **The clone is paid only when a buffered row is actually dropped.** It is O(buffered) —
        // the term the deny-ack memo measured at 165 ms p50 with 1 M buffered — and this is the
        // deny lane, so paying it per window would put a flush-sized stall in front of every
        // revocation. Deleting an entity that already has geometry, which is the ordinary case,
        // costs one hash lookup per entry and no clone at all.
        let buffered_deletions: Vec<EntityId> = deleted
            .into_iter()
            .filter(|entity| generation.buffer.contains(*entity))
            .collect();
        let buffer = if buffered_deletions.is_empty() {
            Arc::clone(&generation.buffer)
        } else {
            let mut buffer = (*generation.buffer).clone();
            for entity in buffered_deletions {
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

    /// Write a side-manifest carrying the live deny state, if any window has moved it.
    ///
    /// **A disc event only.** No geometry moves, nothing is superseded, no cache is pruned and the
    /// generation is untouched — this exists so that a restore from bundle and object store, with
    /// no WAL, recovers the deny state as of the last publication (deny lifecycle memo §4). The
    /// live node never reads it back: its own WAL is authoritative, and `reconstruct` replays over
    /// this as a seed.
    ///
    /// **Off the ack path.** Every 200 in the burst was already sent, at its own window's swap, so
    /// nothing here is between a caller and its acknowledgement. Architecture §3 (r23) budgets the
    /// write path at seconds to minutes with the one condition that a deny's ack stay coupled to
    /// its *application* — which is upstream of this, at the window's fsync and swap.
    ///
    /// **Gated on durability.** A poisoned WAL or a diverged overlay publishes nothing: the
    /// overlay then holds dispositions no durable record backs, and writing them would make a
    /// 500'd, never-acked deny permanent on every restore. The dirty flag survives the refusal, so
    /// a repaired node publishes on its own within a tick rather than waiting for its next deny.
    ///
    /// **Failure alarms and retains.** Nothing is un-acked and nothing is unwound — the state is
    /// WAL-durable either way. Only the disaster-path bound degrades while the alarm stands, and
    /// any later write carries complete state, so a single success repairs it.
    /// Restate the live row-less state into a side-manifest about to be committed: the registry
    /// and its low-water mark, the roster, the runtime attribute columns, the runtime vocabularies
    /// with their values, and the view groups and plain views.
    ///
    /// **Restated from live state, never carried forward from the clone.** The manifest a
    /// publication starts from may be several publications behind, so a registration, a create or
    /// a declaration that landed since would be dropped by carrying it forward — and a rotation
    /// then makes that permanent. `min`, not `max`, for the mark: the row-less region grows
    /// downward.
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

    /// Reclaim what the publication just made redundant — **after** the generation swap and never
    /// before it.
    ///
    /// ```text
    /// generation swap                             ← the publication event, already done above
    /// rotation: snapshot written, then reclaim    ← §7.2, snapshot before any deletion
    /// ```
    ///
    /// **The reclaim bound is the buffer's oldest surviving row.** Rows acked *during* the flush
    /// were appended after its snapshot point, were never consumed, and carry entity ids at or
    /// above the new watermark; reclaiming past them would delete them and recovery would then
    /// reconstruct them from nothing — acked ingest, silently lost at the next restart.
    /// `IngestBuffer::oldest_wal_pos` answers it from the post-publication buffer — the rows that
    /// still have no geometry — and refuses (`None`) if any of them does not know its own
    /// position, which reclaims nothing rather than guessing. With an empty buffer the whole
    /// durable prefix is reclaimable.
    ///
    /// (A `Flush{n, wal_pos}` WAL record used to be appended here first. It was write-only —
    /// recovery reconstructs the buffer by the has-a-row predicate and this function computes its
    /// own bound — and was deleted with `WAL_VERSION` 4 rather than carried as archaeology.)
    ///
    /// **Two gates, and neither is the one `plan_flush` applies.** A poisoned WAL cannot be appended
    /// to at all. A node whose overlay has diverged from its durable WAL must rotate nothing (§7.2):
    /// `Wal::discard_undurable` deliberately does not un-apply, so such a node holds dispositions no
    /// record backs, and writing a snapshot from that overlay would make a 500'd, never-acked deny
    /// permanent. `plan_flush` refuses for the same reason, but it is a different site and a flush
    /// already in flight when the divergence happened reaches here regardless.
    ///
    /// Nothing here is fatal. A failure leaves the log longer than it needs to be, which the next
    /// tick retries; the publication itself is already durable and already swapped.
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
        // A stepped-down node reclaims nothing (owner-ruled 2026-08-04, with the ingest and
        // plan gates): its WAL members are the only recovery material for whatever the
        // step-down shadowed, and freezing reclamation is the fail-closed direction while an
        // operator repairs the damaged newest manifest.
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
            // Nothing buffered: every ingest row has geometry, so everything below the current
            // position — the whole durable prefix — is reclaimable.
            None => self.wal.position(),
            Some(Some(oldest)) => oldest,
            // A buffered row of unknown position pins the log. Fail-safe and loud by construction:
            // the sequence grows, which is visible, rather than a record vanishing, which is not.
            Some(None) => 0,
        };
        // **The oldest artifact publication pins the log too, and today that means from the first
        // publication onwards.** A membership has no home outside the WAL — segments carry rows and
        // postings, manifests carry the registry, and neither carries a Roaring bitmap of who
        // belongs to a cluster — so reclaiming a member holding one destroys the only copy, leaving
        // the artifact registered, still addressable by a `tessera_id` a caller holds, and served
        // as absent. ⊘ Where membership lives on disk is the owner's open decision; until it lands
        // this is the fail-closed direction, and a log that grows is noticed where a membership
        // that vanishes is not.
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
                    // **The idempotency index follows the log it caches.** A restart rebuilds it
                    // from the surviving members, so the entries whose records lay in the members
                    // just deleted go now — otherwise this process would answer a batch id as a
                    // replay that the same node would call unknown after a restart (§2.4).
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

    /// Rotate at the tick when the log has grown and no flush publication is coming to do it —
    /// **the deny-only regime's rotation** (owner-ruled 2026-08-04; write-path §4.5).
    ///
    /// Rotation used to run only inside `publish_flush`, so a node that took denies without ever
    /// flushing — a loaded bundle with no live ingest, the natural state after a bulk load —
    /// sealed nothing, snapshotted nothing and reclaimed nothing: an unbounded log on the one
    /// lane that structurally cannot be shed, replayed in full at every restart. The tick
    /// already fires every `flush_max_age_secs` regardless of buffer contents, so it is the
    /// site.
    ///
    /// **Gated on growth**, so an idle node rotates nothing: a rotation writes an O(overlay)
    /// snapshot and a new member, and doing that per tick on a quiet deployment would be churn
    /// for no reclaim. The position check is exact — append order is sequence order — and
    /// `rotate_wal` re-checks every safety gate (poisoned, diverged, stepped-down) itself.
    /// Safety is the flush-publication rotation's own argument, unchanged: the snapshot
    /// re-states the whole overlay before anything is deleted, and the reclaim bound is the
    /// oldest surviving buffered row, so nothing acked is lost at any crash point.
    pub(super) fn rotate_if_grown(&mut self) {
        if self.wal.position() == self.wal_position_at_last_rotation {
            return;
        }
        self.rotate_wal();
    }

    /// Read the WAL's size and its rotation bound into [`ExecutorHealth::wal_gauge`], at most once
    /// per tick period.
    ///
    /// **A read of state this thread already owns, and nothing else.** It rotates nothing,
    /// compares nothing against a limit and returns no decision: `wal_hard_limit_bytes` is a
    /// startup relation and what a node should do at a runtime ceiling is undecided
    /// (`Wal::disc_bytes`). Nothing in the process reads the gauge it writes; `/control/status`
    /// and the tests are its only consumers.
    ///
    /// **The cost is O(members): two `stat`s per surviving member**, the log file and its `.sync`
    /// sidecar, plus one artifact-store lock and two O(1) reads. Steady-state retention is two
    /// members, and the count is published beside the bytes so a reader can see when the walk
    /// stopped being cheap.
    ///
    /// **The rate limit is what keeps that cost bounded, because the member count is not.** Under
    /// a `growth` or `fill` pin nothing below the pin is reclaimed, so a member accumulates per
    /// rotation for as long as the fold that would release it is refused — the condition this
    /// gauge exists to make visible. The walk therefore gets dearer as the problem gets worse.
    /// The tick it sits on is not a period either: `rows_due` holds continuously while the buffer
    /// is at `flush_max_items`, so a loader the flush cannot keep up with ticks at
    /// `FLUSH_COMPLETION_POLL`, 50 times a second. The clock below bounds the walk to one per
    /// `flush_max_age_secs` whatever the tick does, which is the freshness [`WalGauge`] already
    /// promises.
    ///
    /// Called from both the flushing and the flush-skipped path, so a node whose flush is stalled
    /// still reports the log growing under it, and once at [`Executor::run`]'s entry so a restarted
    /// node does not report an unsampled zero for its first period.
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

    /// Publish new geometry: check, swap, prune. **The executor's own arm of lifecycle §1.3's
    /// swap-only publication step.**
    ///
    /// This ran in `Engine::publish_geometry` until flush needed a second publisher and made the
    /// arrangement untenable. It was a compare-and-swap in a retry loop there — safe against
    /// *itself*, but not against this thread's unconditional `store`, which could clobber a
    /// publication it had already observed. On this thread there is nothing to race, so there is
    /// no loop: one load, one check, one store.
    ///
    /// `check_publishable` is evaluated against the generation actually being replaced, which is
    /// the one loaded here, because this is the only thread that can replace it.
    ///
    /// # The one swap, and everything that rides it
    ///
    /// Compaction §4 step 6: prefix, `segments_version`, watermark, bundle, dictionary and tier
    /// list always; and, when the publication carries a `PrefixRotation`, the base postings, the
    /// fragment cache and the identity it keys, the external-id sidecar, and the retirement of the
    /// executed deletions — all through the single `store` below. Not a sequence of stores that a
    /// request could land between: a request loads one pointer and gets a geometry, a term index,
    /// a fragment identity and a sidecar that agree.
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

        // **Rule F, in the fold's own swap and nowhere else** (write-path §5.4). An entry
        // withdrawn while the old geometry is still live re-exposes the item for the width of that
        // window, so the overlay is cloned, retired against, and published — never mutated in
        // place on a shared `Arc`, which the read path is holding.
        //
        // `overlay_version` moves **only** when something actually retired. A geometry-only swap
        // that bumped it would falsely signal a change on lifecycle §1.2's *security-state* axis,
        // which §8.5's cache keys read; a retirement that did not bump it would be a real change
        // to that state, invisible to the same keys.
        let (overlay, overlay_version) = match rotation.as_ref().map(|r| &r.retired) {
            Some(retired) if !retired.is_empty() => {
                // **The live external-id map loses the retired bindings first** — see
                // `LiveState::forget_established` for why before the retirement rather than after,
                // and for what a binding left standing costs (a lawful re-ingest, refused 409,
                // permanently).
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
            // **A rotation carries the new prefix's own columns**, opened over it by
            // `open_rotation`; every other publication stays within the live prefix and carries
            // the live ones. Cloning the previous generation's across a rotation would serve the
            // superseded prefix's mappings — pre-fold values, the blanking missing, out of files
            // the reclamation is about to unlink (`filter-index.md` §6.2).
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
        // **Listed before the swap, deleted after it** (compaction §8). At this instant every
        // persisted fragment is under the identity about to be superseded, so the listing *is* the
        // set §8 names — which is not selectable by name, since a cache entry is a SHA-256 over the
        // identity and a hash does not invert. Taking it here and deleting below closes both
        // hazards at once: a listing taken before the swap can never name an entry a request wrote
        // after it, and nothing is deleted at all if the swap does not happen.
        let superseded = rotation
            .as_ref()
            .map(|_| previous.fragments.superseded_entries())
            .unwrap_or_default();

        let next = Arc::new(next);
        self.publish_arc(Arc::clone(&next), started);

        // **A rotation refreshes after the swap and does not arm the shed** (decision 0053). The
        // pass still runs, most-recently-used first, so a resident session's projection is rebuilt
        // proactively rather than on its next request — but it is no longer load-bearing, and
        // `refresh.in_flight` is deliberately not set: shed only while the refresh pass is shorter
        // than the rebuild it would save, and a fold inverts that by two orders. After a fold a
        // missing projection is an ordinary cache miss. The rule is stated at
        // `RefreshDeps::in_flight`, which is where a future publication kind will look for it.
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

        // The retention pass, at the swap rather than at a reclaim — see
        // `RowProjectionCache::prune_generations_below` for why depth 1 rather than depth 0, which
        // would delete the input to the very patch it exists to enable.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        Ok(())
    }

    /// The generation swap. **The only `store` in the write path.**
    ///
    /// `load_full` + `store` is safe here for one reason and one only: this is the sole thread that
    /// can publish. A flush would be a second publisher and **must not `store` directly** — it
    /// submits a command and is applied here, as lifecycle §1.3 requires ("submitting a completed,
    /// immutable result back to the lifecycle thread for a swap-only publication step"). A flush
    /// that stored directly would lose geometry publications, and a lost one leaves the
    /// *live* generation on the pin drain list, where the cache's prune evicts projections still in
    /// use. `scripts/check-layers.sh` refuses any non-atomic `.store(` in this crate's sources
    /// outside this file — and that rule is demonstrated going red, not merely written.
    pub(super) fn publish(&self, next: Generation, started: std::time::Instant) {
        self.publish_arc(Arc::new(next), started)
    }

    /// [`Self::publish`] over a generation the caller already holds by `Arc` — a geometry
    /// publication needs the same value afterwards, to hand the background refresh.
    pub(super) fn publish_arc(&self, next: Arc<Generation>, started: std::time::Instant) {
        self.generation.store(next);
        // The overlay/buffer clone above is O(total buffered items). This counter is what makes
        // the deny-ack floor measurable rather than asserted — see
        // `ExecutorHealth::apply_nanos_total`.
        self.health
            .record_apply(started.elapsed().as_nanos() as u64);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Swap);
        }
    }

    /// Mirror the WAL's own poison flag into the posture, **in both directions**.
    ///
    /// Asked of the WAL rather than remembered from the last error this loop happened to see: a
    /// posture derived from the executor's bookkeeping can drift from the thing it describes. That
    /// was the stated intent from the start and it was not what the code did — the flag was raised
    /// through a monotone `fetch_max`, so it could be entered and never left, and the WAL returning
    /// to health was invisible. It is a plain store now, and the WAL is the only thing that decides:
    /// a torn handle never reports healthy because it never *becomes* healthy, not because anything
    /// here refuses to lower the flag.
    pub(super) fn observe_wal(&self) {
        self.health.mirror_wal(self.wal.is_poisoned());
    }

    /// Send a successful receipt.
    ///
    /// The [`PauseSite::BeforeAck`] point is armed **here**, one statement above the send, rather
    /// than at either call site. That is what makes it a statement about the ack rather than about
    /// a line number: an ack that any later rewrite moves above the swap takes this pause point
    /// with it, and a test parked here then observes the effect *not* in force.
    pub(super) fn ack(&self, respond: &Responder, ack: Ack) {
        self.pause_point(PauseSiteArg::BeforeAck);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Ack);
        }
        respond.ack(ack);
    }

    /// Send a failure receipt. **Not** armed with the pause point above: parking there would stall
    /// the WAL-failure tests inside a path that has nothing to say about ack ordering, and there is
    /// no effect for a parked test to look for.
    pub(super) fn ack_failed(&self, respond: &Responder, error: ExecError) {
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Ack);
        }
        respond.fail(error);
    }

    /// Reach an armed pause site, if any. Fault-injection builds only; a no-op otherwise.
    ///
    /// The sites are `faults::PauseSite`'s, which is where each is argued: two discriminate the
    /// ack contract's ordering, and three park this thread at the write path's publication seams
    /// for the correctness suite's crash modifier. Every call site holds no lock — a pause inside
    /// one would wedge this thread against its own waiters.
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
/// In a fault-injection build this **is** [`tessera_lifecycle::faults::PauseSite`]. In a shipped
/// build the module does
/// not exist, so it is a local zero-variant-cost stand-in and `pause_point` is a no-op — the
/// alternative, `#[cfg]` at each call site, is the footgun `faults`'s module doc warns about.
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

/// The two rules the promotion design added to publication (`2026-08-03-descriptor-promotion-design`
/// §2), tested where they are decided rather than through a second view no build produces.
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

    /// **Obligation 9.** One plan is dispatched per tick, because a second unit in flight is
    /// discarded at its rebase. The one sent is the view whose oldest waiting row is oldest — not
    /// the first by name, which is what `views_of`'s lexicographic sort would give and which would
    /// let a continuously-fed `s0` deny `s1` a flush for ever.
    ///
    /// **Mutation:** replace this with `plans.into_iter().next()` and the assertion below fails —
    /// which is the starvation, made into a test.
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

    /// **Obligation 10.** The dictionary guard is scoped to flushes that wrote an extent. A flush
    /// that promoted nothing names only ordinals below the length it planned against, which
    /// append-only extension preserves, so discarding it would cost liveness and buy no safety.
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
