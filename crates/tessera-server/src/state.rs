//! Shared server state: the engine, and the per-token session registry. The registry is the
//! server's responsibility rather than the engine's — the engine mints a [`Session`] and never
//! checks its deadline again, so retention, expiry refusal and revocation all live here.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use tessera_engine::{Engine, Session};

use crate::error::ApiError;

/// One authorised session: the engine's [`Session`], and nothing else.
///
/// **No per-session handle table is held here.** The viewer plane carries `tessera_id` directly
/// and mints no handles, so there is nothing to put in one
/// (docs/decisions/0032-delete-the-dead-handle-table.md). Node handles, which genuinely are
/// per-session, want exactly this seam — held alongside, not inside, `Session`, because
/// `tessera-wire` must not depend on `tessera-engine`'s `EntityId` — and the type they need, with
/// the constraint it records, is `tessera_wire::handles::HandleTable`.
pub struct SessionEntry {
    pub session: Session,
}

/// The current Unix second, as `Session::expires_at` measures it.
///
/// One function rather than the expression inlined at each site, because two readings of the same
/// deadline must not be able to disagree about *which* clock they are on: session expiry is a wall
/// clock throughout (the deadline is minted from the system clock by the engine), so a monotonic
/// `Instant` is not an available substitute here even though it is the right choice for the pin
/// drain list, which times an interval rather than reaching a timestamp.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

/// The retained-session count at which a sweep runs, when twice the live set is smaller.
///
/// **Not load-bearing, and the honest reason it exists is not "cost".** An O(n) scan at n ≤ 16 is
/// nothing; the floor is here so that [`SessionRegistry::insert`] does not run a sweep on literally
/// every call while the registry holds one or two sessions, and so the threshold is never zero.
/// What the number *does* fix is the residue a quiescent process keeps: with the live set below it,
/// up to this many expired sessions — and the fragments they pin — survive until the next
/// authorisation. 16 is twice `serve.expected_concurrent_sessions`' default of 8, which is the
/// concurrency the cache bounds are already sized against, so the sweep's own slack is of the same
/// order as the working set the deployment declared rather than a number chosen for roundness.
const SWEEP_FLOOR_ENTRIES: usize = 16;

/// Every live session, indexed both by bearer token (the viewer plane's lookup) and by
/// `token_id` (`/session/revoke`'s request shape, R5).
///
/// # Why the registry has to shed, and what a retained session actually costs
///
/// An expired session is refused, not forgotten: [`AppState::authenticated_session`] compares the
/// deadline and answers 403. Refusing is an authorisation act and it is complete on its own; what
/// it does not do is give the memory back. Without a sweep both maps grow for the life of the
/// process at one entry per `/session/authorise` call, and the growth is attacker-driven for anyone
/// holding the session credential.
///
/// The cost is not the map entry. Each retained [`SessionEntry`] holds a `Session`, which holds an
/// `Arc<FrozenFragment>` — a live memory mapping. `FragmentCache`'s byte bound governs *its own
/// map*, so evicting an entry there frees nothing while a session still references it (see
/// `FrozenFragment`'s `CacheWeight` impl). **Dead sessions pin exactly the memory that bound exists
/// to release**, which is why this is a memory mechanism rather than tidiness.
///
/// The engine holds a second set of per-session structures under the same `token_id`: the row
/// projection, the masked-count histogram, the occupancy rungs, the derived geometry and the
/// suggest sets, reachable by the token id and by nothing else. [`Self::sweep_expired`] returns
/// the ids it dropped and `/session/authorise` hands each to `Engine::prune_token` — the same call
/// `/session/revoke` makes, so a session that ends by expiring costs the engine what a revoked one
/// costs. Without it those entries would sit until a byte bound chose them, which for a 512 B
/// occupancy rung is many publications away.
///
/// # When the sweep runs, and what bounds the pause
///
/// It runs on **insert**, and on nothing else. Growth is the thing being bounded and insert is the
/// only path that grows the registry, so the trigger sits on the event it is chasing. A sweep is a
/// full pass over `by_token` under the same mutex the viewer plane takes on every request, so it is
/// throttled: the threshold after a sweep is `max(2 × live, SWEEP_FLOOR_ENTRIES)`, which gives
///
/// - **amortised O(1) per authorisation** — each pass over `n` entries is preceded by at least
///   `n/2` inserts that did not sweep;
/// - **retention bounded at `2 × live + 1`**, or the floor, whichever is larger, so the registry
///   can never hold more than twice the sessions that are actually usable;
/// - a **worst-case pause of O(retained) under the request-path lock** — a hash-map scan with no
///   allocation and no IO. That is only acceptable because `retained` is *observable*: it is
///   published on `/control/status` beside `sweeps` and `swept_total`, so an operator can see the
///   n this pass is O(of) rather than infer it. A pass whose n is unobservable is not admissible
///   here however cheap it looks.
///
/// # What it deliberately does not do
///
/// **It is not a timer, and a quiescent process keeps its last generation of dead sessions.** A
/// periodic task would have to be spawned by whoever builds the runtime, so the property would hold
/// under `tessera serve` and not under `mount_server`, an embedder, or a test — the same
/// "sufficient only where this crate builds the runtime" objection `control::DENY_RUNTIME` records
/// against derived arithmetic. Growth-triggered, the sweep is a property of this type: it needs no
/// runtime, it is deterministic, and a test observes it without waiting on a clock. The cost is
/// stated rather than hidden — after the last authorisation up to `2 × live + floor` expired
/// entries remain until the next one. That residue is bounded, it is not growing, and nothing is
/// building new fragments to contend with it, which is the regime in which retention stops
/// mattering.
///
/// **It is not an authorisation mechanism, and nothing may make it one.** The deadline check in
/// [`AppState::authenticated_session`] is what refuses an expired session, and it holds whether or
/// not a sweep has run; [`Self::revoke`] removes a session immediately and no sweep can delay,
/// defer or undo that — a sweep only ever removes. The two directions matter differently: a sweep
/// that ran late would cost memory, whereas a revocation that ran late would be fail-open, so
/// revocation is never routed through this machinery.
///
/// **It does not remove an expired session at the point of refusal**, which would be O(1) and is
/// tempting. Once an entry is gone the next presentation of that token is `bad-credential` (401)
/// rather than `expired-token` (403), so evicting at the refusal would make the 403 unobservable on
/// any retry — a diagnostic loss on precisely the path that tells a client to re-authorise, for
/// memory the throttled sweep already reclaims. The 403 is best-effort either way: **once a session
/// is swept it is indistinguishable from one that never existed**, and no client may treat the
/// 401/403 split as a statement about whether a token was ever valid.
pub struct SessionRegistry {
    by_token: FxHashMap<String, std::sync::Arc<SessionEntry>>,
    token_id_to_token: FxHashMap<u64, String>,
    /// Retained count at which [`Self::insert`] runs a sweep. See [`next_sweep_threshold`].
    sweep_at: usize,
    sweeps: u64,
    swept_total: u64,
}

impl Default for SessionRegistry {
    /// Hand-written rather than derived: a derived `Default` would leave `sweep_at` at zero, which
    /// sweeps on every insert from an empty registry — correct, but not the throttle this type
    /// documents, and the difference would be invisible.
    fn default() -> Self {
        SessionRegistry {
            by_token: FxHashMap::default(),
            token_id_to_token: FxHashMap::default(),
            sweep_at: SWEEP_FLOOR_ENTRIES,
            sweeps: 0,
            swept_total: 0,
        }
    }
}

/// The retained count at which the sweep after this one should run, given the live set it just
/// left behind.
///
/// Doubling is what makes the sweep amortised O(1) per insert; the floor is what keeps it off a
/// registry too small to be worth scanning. `saturating_mul` rather than `*`: overflow here would
/// produce a *small* threshold, i.e. it would sweep more often than intended — harmless in
/// direction but silently not the documented policy, and this file's discipline is that arithmetic
/// on an attacker-influenced count is checked even where the wrap is benign.
fn next_sweep_threshold(live: usize) -> usize {
    live.saturating_mul(2).max(SWEEP_FLOOR_ENTRIES)
}

/// Drop from the token-id index every entry whose token `live` rejects, and return the ids
/// dropped.
///
/// A free function over the index alone, rather than three lines inside
/// [`SessionRegistry::sweep_expired`], because the ids it returns are what the engine is told to
/// prune and they are the one part of the sweep a test can reach: a [`Session`] owns an
/// `Arc<FrozenFragment>`, which only the engine's cache can produce, so no unit test can build the
/// map the rest of the sweep walks.
fn prune_index(index: &mut FxHashMap<u64, String>, live: impl Fn(&str) -> bool) -> Vec<u64> {
    let mut dropped = Vec::new();
    index.retain(|token_id, token| {
        let keep = live(token);
        if !keep {
            dropped.push(*token_id);
        }
        keep
    });
    dropped
}

/// What the registry has retained and reclaimed — `/control/status`'s `sessions` block.
pub struct SessionRegistryStats {
    /// Sessions currently retained: **live and expired-but-not-yet-swept alike**. This is the `n`
    /// the sweep's O(n) pass is over, which is why it is published.
    pub retained: usize,
    /// Sweeps run since process start.
    pub sweeps: u64,
    /// Entries the sweeps have removed, in total. `retained` rising while this stays at zero is the
    /// signature of a registry that is not shedding; both rising together is the mechanism working.
    pub swept_total: u64,
    /// The retained count at which the next sweep runs — `max(2 × live, 16)` as of the last one.
    /// Published so the bound above `retained` is readable rather than a number in a doc comment.
    pub sweep_at: usize,
}

impl SessionRegistry {
    /// Insert a freshly authorised session, and sweep if the registry has grown past its threshold.
    ///
    /// `now_secs` is the caller's Unix-second reading, passed in rather than read here so that the
    /// clock is consulted once per request at the handler and so this method is a pure function of
    /// its inputs. It is a *wall-clock* second because `Session::expires_at` is one; a monotonic
    /// clock cannot be compared against a deadline minted from the system clock.
    ///
    /// Returns the new entry and **the token ids the sweep removed**, which the caller passes to
    /// `Engine::prune_token` once it has dropped this registry's lock. Returned rather than pruned
    /// here for two reasons: the engine is not this type's to reach, and the prune cancels a stage
    /// and walks five caches, which is not work to do under the mutex every viewer request takes.
    pub fn insert(
        &mut self,
        session: Session,
        now_secs: u64,
    ) -> (std::sync::Arc<SessionEntry>, Vec<u64>) {
        let token = session.token.clone();
        let token_id = session.token_id;
        let entry = std::sync::Arc::new(SessionEntry { session });
        self.by_token
            .insert(token.clone(), std::sync::Arc::clone(&entry));
        self.token_id_to_token.insert(token_id, token);
        let expired = if self.by_token.len() >= self.sweep_at {
            self.sweep_expired(now_secs)
        } else {
            Vec::new()
        };
        (entry, expired)
    }

    /// Drop every session whose deadline has passed.
    ///
    /// The predicate is **exactly** [`AppState::authenticated_session`]'s, negated: that method
    /// refuses when `now >= expires_at`, so an entry survives here iff `expires_at > now`. Stated
    /// as an equality rather than a "safe margin" on purpose — a sweep looser than the refusal
    /// would retain memory the refusal has already written off, and a sweep tighter than it would
    /// remove a session that is still being served, turning a 403 into a 401 early. Neither is a
    /// security difference, and that is the point: this pass can only ever remove what the
    /// authorisation check would already refuse.
    ///
    /// Returns the swept token ids, so the caller can give the engine the same treatment a
    /// revocation gives it. A swept session's row projection, masked-count histogram, occupancy
    /// rungs, derived geometry and suggest sets are keyed by `token_id` and reachable by nothing
    /// else once the registry has dropped the token, so without the prune they are held until a
    /// byte bound evicts them — and the occupancy memo and the suggest sets are cheap enough per
    /// entry that a bound is a long way off.
    fn sweep_expired(&mut self, now_secs: u64) -> Vec<u64> {
        let before = self.by_token.len();
        self.by_token
            .retain(|_, entry| entry.session.expires_at > now_secs);
        // The secondary index is pruned against the primary map rather than swept on its own
        // deadline, so the two cannot disagree about which sessions exist — `revoke` reaches
        // `by_token` only through this index, and an index entry outliving its session would make
        // a revocation a silent no-op. The ids dropped here are the ones the engine is told about.
        let by_token = &self.by_token;
        let swept = prune_index(&mut self.token_id_to_token, |token| {
            by_token.contains_key(token)
        });
        self.sweeps += 1;
        self.swept_total += (before - self.by_token.len()) as u64;
        self.sweep_at = next_sweep_threshold(self.by_token.len());
        // The two maps hold one entry per session each and every mutation touches both, so their
        // lengths are equal at rest. Asserted rather than commented because the failure it catches
        // is silent: an index left unpruned would keep growing while `retained` — which reads
        // `by_token` — reported the sweep working. No `/control/status` field can see it, so this is
        // the only mechanism available. Debug-only: it is a statement about this file's own
        // arithmetic, not a runtime guard against a caller.
        debug_assert_eq!(
            self.by_token.len(),
            self.token_id_to_token.len(),
            "the token-id index must be pruned with the session map, or half the registry leaks"
        );
        swept
    }

    pub fn get(&self, token: &str) -> Option<std::sync::Arc<SessionEntry>> {
        self.by_token.get(token).cloned()
    }

    /// Revoke by `token_id` (R5): a token id this registry never minted, or already revoked, is
    /// simply a no-op — `/session/revoke` is 204 either way (revoking twice is not an error).
    ///
    /// **Immediate, and independent of the sweep.** Removal happens in this call; nothing defers it
    /// to a later pass, and the sweep cannot reinstate an entry because it only ever removes.
    pub fn revoke(&mut self, token_id: u64) {
        if let Some(token) = self.token_id_to_token.remove(&token_id) {
            self.by_token.remove(&token);
        }
    }

    /// Sessions currently retained — **live and expired-but-not-yet-swept alike**.
    pub fn len(&self) -> usize {
        self.by_token.len()
    }

    /// Whether any session is retained. Present because clippy asks for it beside [`Self::len`];
    /// `len() == 0` is the meaningful reading, not this.
    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }

    pub fn stats(&self) -> SessionRegistryStats {
        SessionRegistryStats {
            retained: self.by_token.len(),
            sweeps: self.sweeps,
            swept_total: self.swept_total,
            sweep_at: self.sweep_at,
        }
    }
}

/// Both `OwnedSemaphorePermit`s a successful [`ComputeGate::admit`] call returns, held together
/// so a caller can move one value into a `spawn_blocking` closure. Dropping this — which
/// happens when the closure returns, panics, or is otherwise finished — is what releases both
/// permits (or the slot alone, after [`Self::release_compute`]), so accounting stays correct
/// even if the client has disconnected: a permit tracks compute completion, never caller
/// interest. Fields are private; beyond `release_compute` a caller has no reason to touch either
/// permit once held, only to keep this alive across the closure's body.
pub struct GatePermits {
    _slot: OwnedSemaphorePermit,
    /// `Some` from admission until [`Self::release_compute`]; `None` marks this request as
    /// having entered its streaming emit phase, which is what the `Drop` impl reads to keep the
    /// gauge exact.
    compute: Option<OwnedSemaphorePermit>,
    /// The gate's [`ComputeGate::streaming`] gauge, incremented at `release_compute` and
    /// decremented at drop — see that field's doc for why the gauge exists.
    streaming: Arc<AtomicUsize>,
}

impl GatePermits {
    /// The streamed viewport's permit split (`streamed-serving.md` §5): release the compute
    /// permit at sweep completion, keep the slot permit for the emit phase. The emit phase runs
    /// at the client's pace, and holding compute for it would let slow-but-healthy readers
    /// starve the gate; the slot keeps the request counted against total admission until the
    /// closure returns. Idempotent, so an error path that runs it again costs nothing.
    pub fn release_compute(&mut self) {
        if let Some(permit) = self.compute.take() {
            drop(permit);
            self.streaming.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for GatePermits {
    fn drop(&mut self) {
        // `compute` is `None` exactly when `release_compute` ran (admission always sets it),
        // so this cannot under- or over-count.
        if self.compute.is_none() {
            self.streaming.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The two-stage admission gate in front of the viewer/session planes' CPU-bound closures
/// (`/v1/viewport`, `/v1/items`, `/session/authorise`). `compute_admission` is DEFINED as a bound
/// on in-flight *requests*, not on runnable CPU: the rayon pool (`compute_threads`) is what bounds
/// the parallel-sweep CPU any one admitted request may fan out across, and this gate deliberately
/// lets the serialise phase oversubscribe up to `compute_admission` (default 4×
/// `compute_threads` — `tessera-server::config::COMPUTE_ADMISSION_MULTIPLIER`'s doc has the
/// measurement) because small requests at this corpus scale are latency-bound on scheduling, not
/// CPU. Never wraps `/healthz`, `/readyz`, `/v1/meta`, `/session/revoke`, or any control-plane
/// route — a suppression must always reach the WAL, gate saturated or not, so the control plane
/// carries its own bound (see [`IngestAdmission`]) rather than sharing this one.
///
/// Two semaphores, not one, because they bound two different things: `slots` bounds *admitted*
/// requests (running + queued) and is acquired non-blocking, so a caller arriving once every slot
/// is taken sheds immediately rather than piling up unboundedly; `compute` bounds *running*
/// compute and is acquired with a timeout, so a caller that got a slot but still can't start
/// running within `admission_timeout_ms` is also shed rather than served arbitrarily late.
pub struct ComputeGate {
    pub compute_admission: usize,
    pub compute_queue: usize,
    pub admission_timeout_ms: u64,
    slots: Arc<Semaphore>,
    compute: Arc<Semaphore>,
    /// Every 429 *this gate* has produced, from either of its own two shed paths (the outer
    /// slots semaphore and the inner compute-timeout semaphore). No per-principal label (SA §9) —
    /// a single process-wide counter, `/control/status`'s `shed_total`.
    ///
    /// **Does not count every 429 the server can return.** The engine's single-flight builders
    /// (`EngineError::ProjectionBuilding`/`FragmentBuilding`) also emit the 429 `backpressure`
    /// code, as [`crate::error::ApiError::SingleFlightBackpressure`], but those sheds happen
    /// *after* this gate has already admitted the request — a distinct mechanism this counter has
    /// no visibility into, and one whose `detail` says so. A caller correlating
    /// `shed_total` against the client-observed 429 rate should expect the latter to be equal or
    /// higher; the gap is the single-flight sheds, not a bug to chase.
    ///
    /// **Decision 0058 moves traffic between the two, and can move it into this one.** A racer on
    /// a cold row projection used to be shed downstream immediately, releasing its permits at
    /// once; it now parks on the build, holding them for up to `serve.single_flight_wait_ms`. Under
    /// cold-start load that is permit occupancy this gate did not previously see, so `shed_total`
    /// can rise on a workload whose *client-observed* 429 rate has fallen. Decision 0059 records
    /// why that occupancy is bounded by the wait budget rather than by a per-principal share of
    /// this gate, and `row_projection_cache.waiters_now` is what makes it visible.
    shed_total: AtomicU64,
    /// Requests in their streaming emit phase: slot held, compute released
    /// ([`GatePermits::release_compute`]). Without it, `status()`'s `waiting` derivation —
    /// admitted minus running — counts every emit-phase stream as phantom queue depth, and
    /// `/control/status` misreports exactly under streaming load. Incremented/decremented by
    /// [`GatePermits`] itself so it can never drift from the permit state it mirrors.
    streaming: Arc<AtomicUsize>,
}

/// `/control/status`'s `compute` block. `in_flight`/`waiting` are derived from the
/// semaphores' `available_permits` at read time, not tracked separately, so they can never drift
/// from what the gate itself believes.
pub struct ComputeGateStatus {
    pub admission: usize,
    pub queue: usize,
    pub in_flight: usize,
    pub waiting: usize,
    /// Emit-phase streams: slot held, compute released. See [`ComputeGate::streaming`].
    pub streaming: usize,
    /// See [`ComputeGate::shed_total`]'s doc: this gate's own two shed paths only, not the
    /// engine's single-flight builders' 429s.
    pub shed_total: u64,
}

impl ComputeGate {
    pub fn new(compute_admission: usize, compute_queue: usize, admission_timeout_ms: u64) -> Self {
        ComputeGate {
            compute_admission,
            compute_queue,
            admission_timeout_ms,
            slots: Arc::new(Semaphore::new(compute_admission + compute_queue)),
            compute: Arc::new(Semaphore::new(compute_admission)),
            shed_total: AtomicU64::new(0),
            streaming: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The two-stage acquire. On success, returns the held permits — move them into the
    /// `spawn_blocking` closure alongside the engine call — and the queue wait in microseconds,
    /// which becomes the `x-tessera-admission-us` header. Every shed path increments
    /// `shed_total` before returning `ApiError::Backpressure`, so every 429 this gate produces is
    /// counted exactly once.
    pub async fn admit(&self) -> Result<(GatePermits, u64), crate::error::ApiError> {
        let start = Instant::now();

        // Stage 1: the outer slots semaphore, non-blocking. A caller that cannot even get a
        // queue slot is shed immediately — no waiting at all.
        let slot = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                return Err(crate::error::ApiError::Backpressure);
            }
        };

        // Stage 2: the inner compute semaphore, bounded by `admission_timeout_ms`. Held past
        // this point only while queued for a compute permit; the `slot` permit above already
        // accounts for this caller as "admitted", so it stays held across the wait too — that is
        // what bounds the queue's total occupancy at `compute_queue`.
        let compute = match tokio::time::timeout(
            Duration::from_millis(self.admission_timeout_ms),
            Arc::clone(&self.compute).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            // `Ok(Err(_))` (the semaphore was closed) can't happen — this gate never calls
            // `close()` — but is handled identically to a timeout rather than unwrapped, since
            // both mean "no compute permit arrived in time".
            Ok(Err(_)) | Err(_) => {
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                return Err(crate::error::ApiError::Backpressure);
            }
        };

        let admission_us = start.elapsed().as_micros() as u64;
        Ok((
            GatePermits {
                _slot: slot,
                compute: Some(compute),
                streaming: Arc::clone(&self.streaming),
            },
            admission_us,
        ))
    }

    pub fn status(&self) -> ComputeGateStatus {
        // `available_permits` on `compute` is what's actually free to run; the number *held* is
        // the complement against the gate's own fixed capacity.
        let in_flight = self.compute_admission - self.compute.available_permits();
        let admitted =
            (self.compute_admission + self.compute_queue) - self.slots.available_permits();
        let streaming = self.streaming.load(Ordering::Relaxed);
        ComputeGateStatus {
            admission: self.compute_admission,
            queue: self.compute_queue,
            in_flight,
            // Everything admitted but neither running compute nor streaming its emit phase is
            // waiting in the queue. The three gauges are read from two semaphores and an atomic
            // at three instants, so a request moving between states mid-read can skew `waiting`
            // by one transiently — an accepted property of a lock-free status read.
            waiting: admitted.saturating_sub(in_flight).saturating_sub(streaming),
            streaming,
            shed_total: self.shed_total.load(Ordering::Relaxed),
        }
    }
}

/// The bound on **concurrent `/control/ingest` handlers**, which is the bound on how many
/// blocking-pool threads ingest can hold.
///
/// # The failure it exists to prevent
///
/// `spawn_blocking` dispatches onto a process-wide **unbounded FIFO** served by a fixed number of
/// threads, and an ingest handler holds one of those threads across the Arrow decode, the plugin's
/// `terms_of_labels` loop, the external-ID sidecar IO **and** its whole blocking wait on the
/// executor's receipt. Unbounded, ingest can therefore occupy the whole pool. The deny lane is
/// insulated from that by its own runtime (`control::DENY_RUNTIME`), but the viewer plane shares
/// the FIFO and has **no timeout on the wait**, so an admitted viewport — one `ComputeGate` had
/// already let through — would *hang* rather than shed. This bound is what prevents that, and
/// `ingest_admission_sheds_before_the_blocking_pool_fills` is what asserts it.
///
/// # Not a second `ComputeGate`
///
/// One semaphore, `try_acquire` only: no queue and no timeout. A submitter parked waiting for
/// admission costs nothing; an *admitted* one holds a blocking thread. Queueing would convert the
/// first into the second, so the control plane either takes the work now or refuses it, and the
/// refusal costs no blocking thread, no queue slot and no WAL byte.
///
/// # Not a `OnceLock`
///
/// `DENY_RUNTIME`'s process-global pattern is right for a `tokio::runtime::Runtime` and wrong here:
/// this must be **per server**, because the integration-test binary runs many servers in one
/// process and a process-global bound would make every ingest test contend with every other.
pub struct IngestAdmission {
    pub bound: usize,
    permits: Arc<Semaphore>,
    /// Every ingest 429 this bound has produced. Process-wide, no per-principal label (SA §9).
    /// Distinct from the *queue-full* 429, which is counted nowhere here because it is produced
    /// inside the engine — `/control/status` publishes `work_depth` for that one instead.
    shed_total: AtomicU64,
}

/// `/control/status`'s `ingest` block.
pub struct IngestAdmissionStatus {
    pub admission: usize,
    pub in_flight: usize,
    pub shed_total: u64,
}

impl IngestAdmission {
    pub fn new(bound: usize) -> Self {
        IngestAdmission {
            bound,
            permits: Arc::new(Semaphore::new(bound)),
            shed_total: AtomicU64::new(0),
        }
    }

    /// Take a permit, or `None` if the bound is reached. **The permit must be moved into the
    /// blocking closure, not held across the handler's `.await`**: `spawn_blocking`'s closure keeps
    /// running (and keeps its thread) after a disconnected client's handler future is dropped, so a
    /// permit released at handler-drop would under-count exactly when the pool is under pressure.
    /// This is [`GatePermits`]' stated rule — a permit tracks compute completion, never caller
    /// interest — applied to a second resource.
    pub fn try_admit(&self) -> Option<OwnedSemaphorePermit> {
        match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn status(&self) -> IngestAdmissionStatus {
        IngestAdmissionStatus {
            admission: self.bound,
            in_flight: self.bound - self.permits.available_permits(),
            shed_total: self.shed_total.load(Ordering::Relaxed),
        }
    }
}

/// At most one `/v1/categories/{column}/suggest` walk in flight per session
/// (`value-suggestion.md` §5.1). Not a queue and not the compute-admission gate: a per-keystroke
/// surface queued behind viewport renders would be unusable, so a second request for a session
/// already walking is shed with `429` before any work runs, rather than waiting its turn.
///
/// One process-wide set of in-flight `token_id`s, guarded by [`SuggestGuard`] so that a dropped
/// request — a disconnected client, same as [`GatePermits`]' own argument — still frees its slot:
/// the guard's removal runs on `Drop`, not on a success path a cancelled future never reaches.
pub struct SuggestAdmission {
    in_flight: Arc<Mutex<FxHashSet<u64>>>,
}

impl SuggestAdmission {
    pub fn new() -> Self {
        SuggestAdmission {
            in_flight: Arc::new(Mutex::new(FxHashSet::default())),
        }
    }

    /// Try to start a walk for this session. `None` means one is already in flight for this
    /// `token_id`, which the caller answers with `429` before touching the engine.
    pub fn try_begin(&self, token_id: u64) -> Option<SuggestGuard> {
        let mut in_flight = self.in_flight.lock();
        if in_flight.insert(token_id) {
            Some(SuggestGuard {
                in_flight: Arc::clone(&self.in_flight),
                token_id,
            })
        } else {
            None
        }
    }
}

impl Default for SuggestAdmission {
    fn default() -> Self {
        Self::new()
    }
}

/// Held for the life of one suggest walk; releases the session's slot on drop, whether the walk
/// finished, errored, or the handler future was dropped out from under it (the same "a permit
/// tracks compute completion, never caller interest" rule [`GatePermits`] states for the compute
/// gate). Owns a cloned `Arc` rather than borrowing `SuggestAdmission`, so it can move into a
/// `spawn_blocking` closure with a `'static` bound.
pub struct SuggestGuard {
    in_flight: Arc<Mutex<FxHashSet<u64>>>,
    token_id: u64,
}

impl Drop for SuggestGuard {
    fn drop(&mut self) {
        self.in_flight.lock().remove(&self.token_id);
    }
}

/// Process-wide server state, shared (behind `Arc`) across every axum handler on every plane.
pub struct AppState {
    pub engine: Engine,
    pub sessions: Mutex<SessionRegistry>,
    /// The allocator's trim cadence and its gauges — see [`crate::memory`]. Process-wide, named
    /// by no principal, and read by `/control/status`' `heap` block.
    pub heap: crate::memory::HeapWatch,
    pub max_k: usize,
    /// `/v1/categories`' page-size ceiling and its default. See `Config::max_category_values`.
    pub max_category_values: usize,
    /// `/v1/categories/{column}/suggest`'s page ceiling and `limit`'s default. See
    /// `Config::max_suggestions`.
    pub max_suggestions: usize,
    /// The suggestion walk's budget. See `Config::max_suggestion_walk`.
    pub max_suggestion_walk: u64,
    /// The cardinality at or under which the suggestion verb takes the per-session set route. See
    /// `Config::max_suggest_set_entities`.
    pub max_suggest_set_entities: u64,
    /// At most one `/v1/categories/{column}/suggest` in flight per session
    /// (`value-suggestion.md` §5.1). Never touched by any other route.
    pub suggest_admission: SuggestAdmission,
    /// The publication vertex cap a shape is held to. See `Config::max_shape_vertices`.
    pub max_shape_vertices: u64,
    /// A `region` leaf's vertex cap. See `Config::max_region_vertices`.
    pub max_region_vertices: u64,
    /// A `region` leaf's boundary-cell budget, published beside it. See `Config::max_region_cells`.
    pub max_region_cells: usize,
    /// `POST /v1/artifacts/browse`'s page-size ceiling and its default. See
    /// `Config::max_browse_rows`.
    pub max_browse_rows: usize,
    /// The viewer/session admission gate. Never touched by the control plane.
    pub compute_gate: ComputeGate,
    /// The control plane's own admission bound. Deliberately **not** `compute_gate`: an ingest
    /// batch durability-syncing must not be throttled by the budget a slow viewport consumes.
    pub ingest_admission: IngestAdmission,
    /// Per-request row cap on `/control/ingest`; over is 422. Checked after the Arrow decode, which
    /// is the earliest point the row count is knowable.
    pub ingest_max_batch_rows: usize,
    /// Buffer occupancy at which `/control/ingest` is refused with a 429 (§1.3). A **distinct**
    /// bound from `ingest_queue_bound`, which bounds queued commands rather than buffered items.
    pub ingest_buffer_max_items: usize,
    /// Per-request body-byte cap on `/control/ingest`; over is 422.
    ///
    /// **Enforced by a `DefaultBodyLimit` layer on the route, not by a length check in the
    /// handler** — see `control::router`. Carried here so the layer and the 422's detail string
    /// read the same number.
    pub ingest_max_batch_bytes: usize,
    /// Per-request body-byte cap on `PUT` and `PATCH /control/layers/{name}/artifacts`; over is
    /// 422. Enforced by the route's `DefaultBodyLimit` as `ingest_max_batch_bytes` is, and
    /// carried here so the refusal and the `limits` block read the same number.
    pub publish_max_body_bytes: usize,
    /// Artifact records per publication; over is 422. See `Config::max_artifacts_per_request`.
    pub max_artifacts_per_request: usize,
    /// Members per growth page, summed over its artifacts; over is 422. See
    /// `Config::max_members_per_request`.
    pub max_members_per_request: usize,
    /// The published bound on an exclusion list (ingest §2.3). Published, not enforced: the
    /// field it bounds is not built. See `Config::max_excluded_per_request`.
    pub max_excluded_per_request: usize,
    /// Runtime half of the trailer's `stage_ns` gate (see `Config::stage_timing`). The other half
    /// is the `bench-timing` compile feature; both must hold.
    pub stage_timing: bool,
    /// The streamed viewport's flush threshold. See `Config::stream_flush_bytes`.
    pub stream_flush_bytes: usize,
    /// One frame send's stall budget. See `Config::stream_write_stall_ms`.
    pub stream_write_stall_ms: u64,
    /// The whole emit phase's wall budget. See `Config::stream_deadline_ms`.
    pub stream_deadline_ms: u64,
    pub session_credential: String,
    pub operator_credential: String,
    /// `serve.dev_cors_origins`. Empty — the default — means the viewer and session routers mount
    /// no CORS layer at all. See [`crate::cors`] for why this is a development affordance and why
    /// the control plane never consults it.
    pub dev_cors_origins: Vec<String>,
    /// `serve.cors_origins` — the production origin list, **read by the viewer router and by
    /// nothing else** (decision 0102). Empty is the default. The session plane does not consult
    /// it: `/session/authorise` is gated by the session credential, which a browser must never
    /// hold, and [`crate::cors::session_layer`] is what makes that structural rather than
    /// remembered.
    pub cors_origins: Vec<String>,
    /// `serve.cors_loopback` — whether a page served from a loopback address is admitted on the
    /// **viewer plane**, as a listed origin is. Read by [`crate::cors::viewer_layer`] and by
    /// nothing else, on `cors_origins`' rule: the session plane's bearer is the credential that
    /// mints tokens, and a loopback page is still a browser page.
    pub cors_loopback: bool,
    /// The write executor's fault switchboard — the faults build only (decision 0071), absent
    /// from the struct in a default build rather than present and inert. The same `Arc` the
    /// executor consults, so `/control/faults/*` arms the thread that actually pauses. Bearer
    /// auth is the router layer's, like every other control route.
    #[cfg(feature = "fault-injection")]
    pub faults: std::sync::Arc<tessera_lifecycle::faults::FaultSwitchboard>,
}

impl AppState {
    /// Bearer-token lookup for the viewer plane: an unrecognised token is `bad-credential` (401);
    /// a recognised-but-expired one is `expired-token` (403) — the engine itself never checks
    /// `expires_at`, so enforcing the deadline is this method's job and nothing else's.
    pub fn authenticated_session(
        &self,
        token: &str,
    ) -> Result<std::sync::Arc<SessionEntry>, ApiError> {
        let entry = self
            .sessions
            .lock()
            .get(token)
            .ok_or(ApiError::BadCredential)?;
        if now_secs() >= entry.session.expires_at {
            return Err(ApiError::ExpiredToken);
        }
        Ok(entry)
    }

    /// Bearer check for the session and control planes' shared-secret credentials, in time
    /// independent of *where* a wrong guess diverges.
    ///
    /// **`token == expected` is not admissible here, however obvious it looks.** `str` equality
    /// short-circuits on the first differing byte *and* on a length mismatch, which is a prefix
    /// oracle over the operator and session credentials — an attacker who can time it recovers the
    /// secret byte by byte, in linear rather than exponential guesses. These two shared secrets are
    /// the whole of the admin and session planes' authentication, so that is not a side channel
    /// worth deferring.
    ///
    /// **What this does instead, and what it still does not claim.** Both sides are hashed to 32 bytes
    /// and the digests compared with a fixed-length XOR-accumulate that has no early exit. Hashing
    /// first is what makes the comparison independent of the credential's *length* as well as its
    /// content — a fold over two byte strings of unequal length cannot be. It is not a defence
    /// against an attacker who can measure the hash itself, and it does not pretend to be; what it
    /// removes is the prefix oracle, which is the part that turns guessing into searching.
    ///
    /// **The viewer plane is deliberately not changed and is fine as it is.** A viewer token is
    /// looked up in a `HashMap` by value ([`Self::authenticated_session`]) rather than compared
    /// against a known secret, and it is 256 bits of `OsRng`, so there is no gradient for a
    /// prefix-prober to climb. Stated here so the next reader does not "fix" it by symmetry.
    pub fn check_bearer(&self, presented: Option<&str>, expected: &str) -> Result<(), ApiError> {
        let Some(token) = presented else {
            return Err(ApiError::BadCredential);
        };
        let presented_digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let expected_digest: [u8; 32] = Sha256::digest(expected.as_bytes()).into();
        // No early exit, and no short-circuiting operator: every one of the 32 bytes is folded in
        // before the single comparison. `|=` rather than `&&` is the whole point.
        let mut diff = 0u8;
        for (a, b) in presented_digest.iter().zip(expected_digest.iter()) {
            diff |= a ^ b;
        }
        if diff == 0 {
            Ok(())
        } else {
            Err(ApiError::BadCredential)
        }
    }
}

#[cfg(test)]
mod session_registry_tests {
    use super::*;

    /// The sweep's two guarantees are both properties of this one expression, and neither is
    /// observable from an HTTP test at the scale one can drive: that the registry never retains
    /// more than twice its live set (plus the floor), and that each O(n) pass is paid for by at
    /// least n/2 inserts that did not sweep.
    ///
    /// A `SessionRegistry` cannot be built in a unit test — a `Session` owns an
    /// `Arc<FrozenFragment>`, which only the engine's cache can produce — so the arithmetic is
    /// tested here and the behaviour over HTTP, in `tests/http_engine_state.rs`.
    #[test]
    fn the_sweep_threshold_doubles_above_the_floor_and_never_falls_below_it() {
        assert_eq!(next_sweep_threshold(0), SWEEP_FLOOR_ENTRIES);
        assert_eq!(next_sweep_threshold(1), SWEEP_FLOOR_ENTRIES);
        // The floor binds up to half itself; above that, doubling does.
        assert_eq!(
            next_sweep_threshold(SWEEP_FLOOR_ENTRIES / 2),
            SWEEP_FLOOR_ENTRIES
        );
        assert_eq!(next_sweep_threshold(100), 200);

        // An absurd live set must not wrap to a *small* threshold, which would sweep constantly
        // rather than never — benign in direction, silently not the documented policy.
        assert_eq!(next_sweep_threshold(usize::MAX), usize::MAX);
    }

    /// The ids the sweep hands to `Engine::prune_tokens` are exactly the ones whose sessions went,
    /// and the index keeps the rest. Both halves are asserted: an over-broad return would prune a
    /// live session's caches, costing it a rebuild on its next request, and a short one would
    /// leave a dead session's entries resident under an id nothing can present again.
    #[test]
    fn the_sweep_returns_the_ids_it_dropped_and_keeps_the_rest() {
        let mut index: FxHashMap<u64, String> =
            (1..=4).map(|id| (id, format!("token-{id}"))).collect();
        let live = ["token-2", "token-4"];

        let mut dropped = prune_index(&mut index, |token| live.contains(&token));
        dropped.sort_unstable();

        assert_eq!(dropped, vec![1, 3]);
        let mut kept: Vec<u64> = index.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![2, 4]);
    }

    /// A sweep that removed nothing returns nothing, so the caller starts no prune and takes no
    /// blocking thread for an empty batch.
    #[test]
    fn a_sweep_that_removes_nothing_returns_nothing() {
        let mut index: FxHashMap<u64, String> =
            (1..=3).map(|id| (id, format!("token-{id}"))).collect();
        assert!(prune_index(&mut index, |_| true).is_empty());
        assert_eq!(index.len(), 3);
    }
}

#[cfg(test)]
mod compute_gate_tests {
    use super::*;

    /// Stage 1 of the gate: with `compute_admission = 1, compute_queue = 0` — the deterministic
    /// configuration — a second concurrent `admit()` while the first permit is still held sheds via
    /// `try_acquire`, with no waiting and no timeout elapsed.
    #[tokio::test]
    async fn a_second_admit_sheds_immediately_when_slots_are_exhausted() {
        let gate = ComputeGate::new(1, 0, 250);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");

        let second = gate.admit().await;
        assert!(
            matches!(second, Err(ApiError::Backpressure)),
            "a saturated gate must shed the second admission"
        );
        assert_eq!(gate.status().shed_total, 1);

        drop(first_permits);
    }

    /// Stage 2 of the gate: a slot is available (queue has room) but the compute semaphore is fully
    /// held, so the second caller waits and is shed only once `admission_timeout_ms` elapses —
    /// exercised with a near-zero timeout so this test does not depend on wall-clock timing to
    /// pass reliably.
    #[tokio::test]
    async fn a_queued_admit_sheds_after_the_admission_timeout() {
        let gate = ComputeGate::new(1, 1, 1);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");

        let second = gate.admit().await;
        assert!(
            matches!(second, Err(ApiError::Backpressure)),
            "a caller that cannot get a compute permit within the timeout must be shed"
        );
        assert_eq!(gate.status().shed_total, 1);

        drop(first_permits);
    }

    /// No permit leak: after a shed, both the slot and compute permits the
    /// shed attempt failed to fully acquire are returned — a fresh `admit()` must succeed again
    /// once the original holder releases, not stay wedged.
    #[tokio::test]
    async fn no_permit_leak_after_a_shed() {
        let gate = ComputeGate::new(1, 0, 1);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");
        assert!(matches!(gate.admit().await, Err(ApiError::Backpressure)));

        drop(first_permits);

        // The shed attempt above must not have left the slots semaphore permanently short a
        // permit -- a third admit, after the only holder releases, must succeed.
        let third = gate.admit().await;
        assert!(
            third.is_ok(),
            "a permit leak would make this admit shed too"
        );
    }

    /// No permit leak on the timeout path specifically: `try_acquire_owned` on the outer
    /// semaphore succeeds (a queue slot is available) but the inner semaphore's timeout expires.
    /// The slot permit `admit` acquired for that failed attempt must still be returned to the
    /// pool, not leaked, or the gate's queue capacity would shrink by one on every timeout shed.
    #[tokio::test]
    async fn no_slot_leak_on_a_compute_timeout_shed() {
        let gate = ComputeGate::new(1, 1, 1);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");
        assert!(matches!(gate.admit().await, Err(ApiError::Backpressure)));

        // Two slots total (admission=1, queue=1); the first holder still has one. A second
        // *queued* admit (which will itself time out, since compute is still fully held) must
        // still be able to acquire a SLOT -- proving the previous shed returned its slot permit.
        let slots_status = gate.status();
        assert_eq!(
            slots_status.waiting, 0,
            "the timed-out attempt must not remain counted as waiting"
        );

        drop(first_permits);
        let fourth = gate.admit().await;
        assert!(fourth.is_ok(), "a slot leak would make this admit shed too");
    }

    /// `/control/status`'s gauges: `in_flight` and `waiting` are derived from the
    /// semaphores' own permit counts, so they must reflect an admitted-and-running permit as
    /// in_flight = 1, waiting = 0, and go back to 0/0 once released.
    #[tokio::test]
    async fn status_reports_in_flight_and_resets_on_release() {
        let gate = ComputeGate::new(2, 1, 250);
        let status = gate.status();
        assert_eq!(status.admission, 2);
        assert_eq!(status.queue, 1);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.waiting, 0);

        let (permits, _) = gate.admit().await.expect("admit must succeed");
        let status = gate.status();
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.waiting, 0);

        drop(permits);
        let status = gate.status();
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.waiting, 0);
    }
}
