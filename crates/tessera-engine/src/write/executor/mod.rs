use super::*;

mod background;
mod commands;
mod deny;
mod fold;
mod manifest;
mod memberships;
mod publications;
mod values;
mod wal;
mod window;

pub(super) use background::Background;

pub use commands::*;
pub use deny::*;
pub(in crate::write) use fold::*;
use manifest::*;
use memberships::*;
pub(in crate::write) use publications::*;
use wal::*;
use values::*;

// =================================================================================================
// The executor
// =================================================================================================

/// How long after a cycle failed to publish the next retry may come. Without this floor a failed
/// cycle would retry as fast as the loop can wake. A period tick and a row trip are not held back
/// by it.
pub(super) const FAILED_CYCLE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

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
        tessera_cache::SingleFlightCache<
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
    pub(super) merge_policy: MergePolicy,
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
    /// See [`MaintenanceDeps::switches`].
    pub(super) switches: Arc<crate::switches::TestSwitches>,
    /// See [`crate::compact::CompactionSchedule`]. Consulted at the tick, beside the flush's own.
    pub(super) compaction: crate::compact::CompactionSchedule,
    /// When the last fold attempt started, as a unix second. Stamped by every dispatch whatever the
    /// attempt then does, so the interval limits attempts.
    pub(super) last_fold_start_unix: Option<u64>,
    /// Every external-id sidecar replaced over the live prefix, weakly held: a `Weak` answers
    /// whether one is still alive without keeping its mappings alive itself. Moved into
    /// [`PendingReclaim`] at a fold.
    pub(super) superseded_sidecars: Vec<std::sync::Weak<crate::engine::ExternalIdIndex>>,
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

/// Why a tick is firing. `due` is the cadence itself, the period or the buffered-row count;
/// `period_due` is the period alone. A tick that only a request brought on is not `due`.
struct TickDue {
    due: bool,
    period_due: bool,
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
        let Some(TickDue { due, period_due }) = self.tick_due() else {
            return;
        };
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
                self.tick_behind_flush(&generation, period_due);
            }
            return;
        }

        self.last_tick = std::time::Instant::now();
        self.health.mark_tick(self.last_tick);

        let (plans, gated) = self.plan_view_flushes(&generation);
        self.dispatch_planned_tick(&generation, plans, gated);
        drop(generation);
        // Last, so a reader that sees the count move sees everything this tick did on this
        // thread: the plans dispatched, the log rotated.
        self.health.ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether this tick fires, and on what. `None` is a wake that is not a tick.
    fn tick_due(&self) -> Option<TickDue> {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        let rows_due = self.health.buffered_items.load(Ordering::SeqCst) >= self.flush_max_items;
        let period_due = self.last_tick.elapsed() >= period;
        let due = period_due || rows_due;
        let requested = self.health.flush_requested.load(Ordering::SeqCst);
        let fold_requested = self.health.fold_requested.load(Ordering::SeqCst);
        if !due && !requested && !fold_requested {
            return None;
        }
        // Floored ([`FAILED_CYCLE_RETRY`]) so a retry does not re-plan the buffer at every wake.
        if !due && self.health.failed_cycle_backoff().is_some() {
            return None;
        }
        Some(TickDue { due, period_due })
    }

    /// The tick that lands while a flush is still running: it stamps the tick and counts what is
    /// waiting, and publishes nothing.
    fn tick_behind_flush(&mut self, generation: &Generation, period_due: bool) {
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

    /// Plan every view's flush, publish the row count they cover, and report whether any view was
    /// refused rather than merely having nothing to flush.
    fn plan_view_flushes(
        &self,
        generation: &Arc<Generation>,
    ) -> (Vec<(String, crate::flush::FlushPlan)>, bool) {
        let mark = StageMark::now();
        let mut flushable = 0usize;
        let mut gated = false;
        let mut plans: Vec<(String, crate::flush::FlushPlan)> = Vec::new();
        for view in views_of(generation) {
            match crate::flush::plan_flush(
                generation,
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
        (plans, gated)
    }

    /// Dispatch what the tick planned: the flushes, and the three background units the tick also
    /// drives.
    fn dispatch_planned_tick(
        &mut self,
        generation: &Arc<Generation>,
        plans: Vec<(String, crate::flush::FlushPlan)>,
        gated: bool,
    ) {
        if plans.is_empty() {
            self.health.deferred_plans.store(false, Ordering::SeqCst);
            self.rotate_if_grown();
            if gated {
                self.health.fail_publication_cycle();
            } else {
                self.health.close_publication_cycle();
            }
        } else if !self.dispatch_flushes(generation, plans) {
            self.health.fail_publication_cycle();
        }
        // Dispatched before the two it suspends, so a tick that starts a fold does not also start
        // a merge that the flip would orphan.
        self.dispatch_fold(generation);
        self.dispatch_coalesce(generation);
        self.dispatch_merge(generation);
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

    /// Block until something may be waiting, and report whether the executor should keep running.
    ///
    /// While the WAL is degraded this also wakes on a timer, since `/readyz` steers traffic away
    /// from a degraded node and recovery would otherwise be reachable only by traffic. Every other
    /// reason to resume is a ring: a submission, a flush request, or a background unit finishing.
    ///
    /// Shutdown leaves only from here, and only once the handler side is gone — which closes both
    /// queues — with both of them already drained by the passes above.
    pub(super) fn wait_for_work(&self) -> bool {
        if self.queues.handler.strong_count() == 0 {
            return false;
        }
        // Bounded by the next tick, always, so a quiescent node still runs reclaim.
        let until_tick = std::time::Duration::from_secs(self.flush_max_age_secs)
            .saturating_sub(self.last_tick.elapsed());
        let wait = if self.wal.is_poisoned() {
            until_tick.min(WAL_RECOVERY_POLL_INTERVAL)
        } else if let Some(backoff) = self.health.failed_cycle_backoff() {
            until_tick.min(backoff)
        } else {
            until_tick
        };
        let _ = self.queues.bell.recv_timeout(wait);
        true
    }

    /// The prefix directory to write into, derived from the generation the caller is publishing
    /// against rather than remembered.
    ///
    /// A fold flips `CURRENT`, changing which prefix is live. A stored `PathBuf` rotated at the
    /// flip would have to be got right at every site that uses it; a derived value cannot be missed.
    pub(super) fn prefix_dir(&self, generation: &Generation) -> PathBuf {
        self.bundle_root.join(&generation.prefix)
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
            g.segments_version = segments_version;
            g.watermark = watermark;
            g.bundle = bundle;
            g.dict = dict;
            // A rotation carries the new prefix's own columns; the previous generation's, which
            // `with` starts from, would serve the superseded prefix's mappings out of files
            // reclamation is unlinking.
            if let Some(r) = &rotation {
                g.filter_columns = Arc::clone(&r.filter_columns);
                g.postings = Arc::clone(&r.postings);
                g.fragments = Arc::clone(&r.fragments);
                g.external_index = Arc::clone(&r.external_index);
            }
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
    BeforeManifestPublish,
    BeforeCurrentFlip,
    BeforeMergePublish,
}
