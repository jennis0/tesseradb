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
pub(crate) use background::outstanding;
pub(super) use manifest::SideManifests;
pub(super) use wal::ExecutorLog;

pub use commands::*;
pub use deny::*;
pub(in crate::write) use fold::*;
use manifest::*;
use memberships::*;
pub(in crate::write) use publications::*;
use wal::*;
use values::*;
use window::mint_values_codes;


/// How long after a failed cycle the next retry may come, so it does not retry on every wake.
pub(super) const FAILED_CYCLE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// The single writer. One per partition, on its own thread, owning the WAL by value.
pub(super) struct Executor {
    pub(super) log: ExecutorLog,
    pub(super) live: Arc<LiveState>,
    /// The only publishing capability in the write path; not in [`LiveState`], which is shared.
    pub(super) generation: Arc<GenerationHandle>,
    /// Pruned of generations older than the retention depth, at the swap.
    pub(super) row_projection_cache: Arc<RowProjectionCache>,
    /// Policies, paths, and the shared pool and caches, fixed at construction.
    pub(super) deps: MaintenanceDeps,
    pub(super) queues: LifecycleQueues,
    pub(super) health: Arc<ExecutorHealth>,
    /// All it has to be is distinct per window.
    pub(super) window_seq: u64,
    /// What this node has published into side-manifests, and what live state holds beyond them.
    pub(super) side_manifests: SideManifests,
    /// Separate from a flush so the cheap one does not wait on it.
    pub(super) coalesce: Background<crate::coalesce::CompletedCoalesce>,
    /// Held separately since it publishes its own swap.
    pub(super) merge: Background<crate::merge::CompletedMerge>,
    /// Runs on its own thread: it takes minutes to hours and the pool serves viewports.
    pub(super) fold: Background<crate::compact::CompletedFold>,
    /// Reads a vocabulary out of the generation and writes files the manifest does not name.
    pub(super) suggest: Background<crate::suggest::CompletedSuggest>,
    /// Its in-flight flag is [`ExecutorHealth::flush_in_flight`], which status reads.
    pub(super) flush: Background<crate::flush::CompletedFlush>,
    /// When the last fold attempt started, as a unix second, so the interval can limit attempts.
    pub(super) last_fold_start_unix: Option<u64>,
    /// Held weakly, so a `Weak` answers if one is alive; moved to [`PendingReclaim`] at a fold.
    pub(super) superseded_sidecars: Vec<std::sync::Weak<crate::engine::ExternalIdIndex>>,
    /// Deleted once nothing holds its generation or sidecar; a dead node leaves it to the sweep.
    pub(super) pending_reclaim: Vec<PendingReclaim>,
    /// So the first tick lands one period after construction, not immediately.
    pub(super) last_tick: std::time::Instant,
    /// What every accepted write since the last tick did to each level's row forms.
    pub(super) pending_forms: std::collections::BTreeMap<(String, u32), Vec<crate::artifacts::LevelDelta>>,
    #[cfg(feature = "fault-injection")]
    pub(super) faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

/// Why a tick fired: `due` is the period or the buffered-row count, `period_due` the period alone.
struct TickDue {
    due: bool,
    period_due: bool,
}

impl Executor {
    /// Drop the region decompositions of generations older than the retention depth.
    pub(super) fn prune_region_cache(&self, segments_version: u64) {
        let floor = segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS);
        self.deps.region_cache
            .retain_keys(|key| key.segments_version >= floor);
    }

    /// Runs the tick, then drains deny to empty, then at most one work item, repeating, blocking
    /// only once both queues are empty. Completed background units publish before the tick plans,
    /// so it never re-plans rows already written; the deny lane drains fully before any work item;
    /// [`Executor::run_work_pass`] returns after closing one window, so a deny waits for at most
    /// one window; shutdown drains and executes rather than discarding.
    pub(super) fn run(&mut self) {
        self.sample_wal_gauge();
        loop {
            self.recover_wal();
            let published = self.publish_completed_flushes()
                | self.publish_completed_coalesces()
                | self.publish_completed_merges()
                | self.publish_completed_folds()
                | self.publish_completed_suggests();
            self.tick_if_due();
            while self.run_deny_pass() {}
            // The prompt half only; a batch's memberships wait for the tick.
            if self.side_manifests.behind_live {
                self.publish_overlay_state();
            }
            if self.run_work_pass() || published {
                continue;
            }
            if !self.wait_for_work() {
                break;
            }
        }
    }

    /// The flush tick: the cadence on which geometry publishes; also drives `reclaim`.
    pub(super) fn tick_if_due(&mut self) {
        let Some(TickDue { due, period_due }) = self.tick_due() else {
            return;
        };
        // A flush handed back after this loop's drain is not yet in the generation, and a plan
        // taken now would carry its rows again at its `row_base`. It is not a tick behind a
        // running flush either: the next loop drains it and ticks straight away.
        let flush_in_flight = self.flush.in_flight();
        if !flush_in_flight && self.flush.completed_pending() {
            return;
        }
        if !flush_in_flight {
            self.health.open_publication_cycle();
        }
        self.sample_wal_gauge();
        // Ahead of the flush's in-flight gate: these wait on nothing this executor does.
        self.reclaim_superseded_prefixes();
        self.dispatch_suggest_rebuild();
        self.publish_row_forms();
        // Above the in-flight gate, so a tick that publishes no geometry still publishes this.
        self.publish_overlay_state();

        let generation = self.generation.load_full();

        // A period tick during a flush is skipped, not queued; a requested flush stays armed.
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
        // Last, so a reader that sees the count move sees everything else this tick did.
        self.health.ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether this tick fires, and on what. `None` is a wake that is not a tick.
    fn tick_due(&self) -> Option<TickDue> {
        let period = std::time::Duration::from_secs(self.deps.flush_max_age_secs);
        let rows_due =
            self.health.buffered_items.load(Ordering::SeqCst) >= self.deps.flush_max_items;
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

    /// Lands while a flush still runs: stamps the tick, counts what is waiting, publishes nothing.
    fn tick_behind_flush(&mut self, generation: &Generation, period_due: bool) {
        self.last_tick = std::time::Instant::now();
        self.health.mark_tick(self.last_tick);
        self.health.ticks.fetch_add(1, Ordering::Relaxed);
        // No deletion filter needed: the buffer never holds rows of a deleted entity.
        let flushable = generation.buffer.len();
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

    /// Plan every view's flush and report whether any view was refused, not merely empty.
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
                self.log.wal.is_poisoned(),
                self.health.overlay_diverged.load(Ordering::SeqCst),
            ) {
                Ok(plan) => {
                    flushable += plan.items.len();
                    plans.push((view, plan));
                }
                Err(crate::flush::NoFlush::NothingToFlush) => {}
                Err(refusal) => {
                    gated = true;
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

    /// Dispatch what the tick planned: the flushes, and the three background units it also drives.
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

    /// Whether this executor may still write durable state, asked in one place. Excludes the
    /// WAL's poison flag, which is recoverable; these two are terminal until a restart.
    pub(super) fn may_publish(&self) -> bool {
        !self.health.overlay_diverged.load(Ordering::SeqCst)
            && !self.health.prefix_diverged.load(Ordering::SeqCst)
    }

    /// Block until something may be waiting, and report whether the executor should keep running.
    /// While the WAL is degraded this also wakes on a timer, since `/readyz` steers traffic away
    /// from a degraded node and recovery would otherwise only be reachable through traffic.
    pub(super) fn wait_for_work(&self) -> bool {
        if self.queues.handler.strong_count() == 0 {
            return false;
        }
        // Bounded by the next tick, always, so a quiescent node still runs reclaim.
        let until_tick = std::time::Duration::from_secs(self.deps.flush_max_age_secs)
            .saturating_sub(self.last_tick.elapsed());
        let wait = if self.log.wal.is_poisoned() {
            until_tick.min(WAL_RECOVERY_POLL_INTERVAL)
        } else if let Some(backoff) = self.health.failed_cycle_backoff() {
            until_tick.min(backoff)
        } else {
            until_tick
        };
        let _ = self.queues.bell.recv_timeout(wait);
        true
    }

    /// Derived at each use, since a fold moves the live prefix.
    pub(super) fn prefix_dir(&self, generation: &Generation) -> PathBuf {
        self.deps.bundle_root.join(&generation.prefix)
    }

    /// Publish new geometry: check, swap, prune. The sole writer, so no compare-and-swap retry
    /// loop: one load, one check, one store.
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

        // Cloned and retired against here, never mutated on the shared `Arc` the read path holds.
        // Cache keys read `overlay_version` as the security state, so a geometry-only swap must
        // not move it.
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
            // A rotation carries the new prefix's own columns, or `with` would serve stale entries.
            if let Some(r) = &rotation {
                g.filter_columns = Arc::clone(&r.filter_columns);
                g.postings = Arc::clone(&r.postings);
                g.fragments = Arc::clone(&r.fragments);
                g.external_index = Arc::clone(&r.external_index);
            }
            g.delta_postings = delta_postings;
            g.overlay_version = overlay_version;
            g.overlay = overlay;
        });
        // Listed before the swap and deleted after it, so nothing is lost if the swap never lands.
        let superseded = rotation
            .as_ref()
            .map(|_| previous.fragments.superseded_entries())
            .unwrap_or_default();

        let next = Arc::new(next);
        self.publish_arc(Arc::clone(&next), started);

        // After the swap, so a missing projection after a fold is an ordinary cache miss.
        if rotation.is_some() {
            self.deps.refresh.spawn(next);
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

        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        Ok(())
    }

    /// The only place a generation is stored; the executor thread is the only publisher, so a
    /// flush submits a command rather than storing directly. `scripts/check-layers.sh` refuses
    /// any non-atomic `.store(` elsewhere in this crate.
    pub(super) fn publish(&self, next: Generation, started: std::time::Instant) {
        self.publish_arc(Arc::new(next), started)
    }

    /// Like [`Self::publish`] but takes an `Arc` directly, for the caller to reuse afterwards.
    pub(super) fn publish_arc(&self, next: Arc<Generation>, started: std::time::Instant) {
        self.generation.store(next);
        self.health
            .record_apply(started.elapsed().as_nanos() as u64);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Swap);
        }
    }

    /// Reach an armed pause site, if any; a no-op outside fault-injection builds.
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

/// So call sites read the same in both builds: [`tessera_lifecycle::faults::PauseSite`] here, a
/// stand-in in a shipped build.
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
