//! Shared server state: the engine, and the per-token session registry Task 11's report flags as
//! the server's (not the engine's) responsibility to own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use tessera_engine::{Engine, Session};

use crate::error::ApiError;

/// One authorised session: the engine's [`Session`], and nothing else.
///
/// **No per-session handle table is held here.** The viewer plane carries `tessera_id` directly
/// and mints no handles, so there is nothing to put in one. Phase 3's node handles are genuinely
/// per-session and want exactly this seam — held alongside, not inside, `Session`, because
/// `tessera-wire` must not depend on `tessera-engine`'s `EntityId` — and the type they need, with
/// the constraint it records, is `tessera_wire::handles::HandleTable`.
pub struct SessionEntry {
    pub session: Session,
}

/// Every live session, indexed both by bearer token (the viewer plane's lookup) and by
/// `token_id` (`/session/revoke`'s request shape, R5).
///
/// # This registry is a third attacker-driven memory path, and nothing here bounds it
///
/// Recorded because Task 5 closed the other two (the projection cache's byte bound and entry
/// floor, and `FragmentCache`'s `key_memo` clear) and this one is not Track C's to close — the
/// expiry sweep is the owner's/Track B's. **An expired session is 403'd but never removed**:
/// [`AppState::authenticated_session`] checks the deadline and refuses, and nothing ever calls
/// [`Self::revoke`] for it, so both maps grow for the life of the process at one entry per
/// `/session/authorise` call. `/session/authorise` is behind the shared session credential, so this
/// is not a viewer-plane exposure; a holder of that secret already has cheaper things to do.
///
/// **The consequence that is not obvious: it defeats the fragment cache's byte bound.** Each
/// retained [`SessionEntry`] holds a `Session`, which holds an `Arc<FrozenFragment>` — a live
/// mapping. `FragmentCache`'s bound governs *its own map*; evicting an entry frees nothing while
/// any session still references it (see `FrozenFragment`'s `CacheWeight` impl). So dead-but-
/// retained sessions pin exactly the memory the new bound was added to release.
///
/// [`Self::len`] is the gauge; `/control/status` publishing it is Track B's wiring, alongside
/// `CacheStats`. A sweep — on a timer, or opportunistically on insert — is the fix, and it is a
/// controller decision, not this track's.
#[derive(Default)]
pub struct SessionRegistry {
    by_token: FxHashMap<String, std::sync::Arc<SessionEntry>>,
    token_id_to_token: FxHashMap<u64, String>,
}

impl SessionRegistry {
    pub fn insert(&mut self, session: Session) -> std::sync::Arc<SessionEntry> {
        let token = session.token.clone();
        let token_id = session.token_id;
        let entry = std::sync::Arc::new(SessionEntry { session });
        self.by_token
            .insert(token.clone(), std::sync::Arc::clone(&entry));
        self.token_id_to_token.insert(token_id, token);
        entry
    }

    pub fn get(&self, token: &str) -> Option<std::sync::Arc<SessionEntry>> {
        self.by_token.get(token).cloned()
    }

    /// Revoke by `token_id` (R5): a token id this registry never minted, or already revoked, is
    /// simply a no-op — `/session/revoke` is 204 either way (revoking twice is not an error).
    pub fn revoke(&mut self, token_id: u64) {
        if let Some(token) = self.token_id_to_token.remove(&token_id) {
            self.by_token.remove(&token);
        }
    }

    /// Sessions currently retained — **live and expired-but-not-swept alike**, which is the whole
    /// reason it is worth publishing. See this type's doc: nothing removes an expired session, so a
    /// number here that only ever rises, while `young_evictions` stays quiet, is the signature of
    /// the retention path rather than of cache pressure. The two gauges answer different questions
    /// and an operator needs both.
    pub fn len(&self) -> usize {
        self.by_token.len()
    }

    /// Whether any session is retained. Present because clippy asks for it beside [`Self::len`];
    /// `len() == 0` is the meaningful reading, not this.
    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

/// Both `OwnedSemaphorePermit`s a successful [`ComputeGate::admit`] call returns, held together
/// so a caller can move one value into a `spawn_blocking` closure (D-B). Dropping this — which
/// happens when the closure returns, panics, or is otherwise finished — is what releases both
/// permits, so accounting stays correct even if the client has disconnected: a permit tracks
/// compute completion, never caller interest. Fields are private; a caller has no reason to touch
/// either permit once held, only to keep this alive across the closure's body.
pub struct GatePermits {
    _slot: OwnedSemaphorePermit,
    _compute: OwnedSemaphorePermit,
}

/// D-B: the two-stage admission gate in front of the viewer/session planes' CPU-bound closures
/// (`/v1/viewport`, `/v1/items`, `/session/authorise`). `compute_admission` is DEFINED as a bound
/// on in-flight *requests*, not on runnable CPU: the rayon pool (`compute_threads`) is what bounds
/// the parallel-sweep CPU any one admitted request may fan out across, and this gate deliberately
/// lets the serialise phase oversubscribe up to `compute_admission` (default 4×
/// `compute_threads` — `tessera-server::config::COMPUTE_ADMISSION_MULTIPLIER`'s doc has the
/// measurement) because small requests at this corpus scale are latency-bound on scheduling, not
/// CPU. Never wraps `/healthz`, `/readyz`, `/v1/meta`, `/session/revoke`, or any control-plane
/// route (D13: a suppression must always reach the WAL, gate saturated or not).
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
    /// **Does not count every 429 the server can return.** D-G's single-flight builders
    /// (`EngineError::ProjectionBuilding`/`FragmentBuilding`, Tasks 1-2) also map to 429
    /// `backpressure` at `map_engine_error`, but those sheds happen *after* this gate has already
    /// admitted the request — they are a distinct mechanism this counter has no visibility into.
    /// A caller correlating `shed_total` against the client-observed 429 rate should expect the
    /// latter to be equal or higher, never a mismatch to chase as a bug (see Task 9's bench report
    /// for a worked example of this exact confusion, isolated via a direct `shed_total`-vs-observed
    /// delta).
    shed_total: AtomicU64,
}

/// `/control/status`'s `compute` block (D-B). `in_flight`/`waiting` are derived from the
/// semaphores' `available_permits` at read time, not tracked separately, so they can never drift
/// from what the gate itself believes.
pub struct ComputeGateStatus {
    pub admission: usize,
    pub queue: usize,
    pub in_flight: usize,
    pub waiting: usize,
    /// See [`ComputeGate::shed_total`]'s doc: this gate's own two shed paths only, not the D-G
    /// single-flight builders' 429s.
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
        }
    }

    /// The two-stage acquire (D-B). On success, returns the held permits — move them into the
    /// `spawn_blocking` closure alongside the engine call — and the queue wait in microseconds,
    /// which becomes the `x-tessera-admission-us` header (D-E). Every shed path increments
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
                _compute: compute,
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
        ComputeGateStatus {
            admission: self.compute_admission,
            queue: self.compute_queue,
            in_flight,
            // Everything admitted but not yet running compute is waiting in the queue.
            waiting: admitted.saturating_sub(in_flight),
            shed_total: self.shed_total.load(Ordering::Relaxed),
        }
    }
}

/// Task 6 (D2): the bound on **concurrent `/control/ingest` handlers**, which is the bound on how
/// many blocking-pool threads ingest can hold.
///
/// # The failure it exists to prevent
///
/// `spawn_blocking` dispatches onto a process-wide **unbounded FIFO** served by a fixed number of
/// threads. Before this, nothing bounded concurrent `/control/ingest` handlers, and each one holds
/// a thread across the Arrow decode, the plugin's `terms_of_label` loop, the external-ID sidecar IO
/// **and** its whole blocking wait on the executor's receipt. Task 3b closed the deny lane's
/// exposure to that by giving `/control/changes` its own runtime; it did not close the class. The
/// viewer plane still shared the FIFO with unbounded ingest and had **no timeout on the wait**, so
/// an admitted viewport — one that `ComputeGate` had already let through — would *hang* rather than
/// shed. That is what this closes, and `ingest_admission_sheds_before_the_blocking_pool_fills` is
/// what asserts it.
///
/// # Not a second `ComputeGate`
///
/// One semaphore, `try_acquire` only: no queue and no timeout. A queued ingest handler is exactly
/// the parked submitter Task 3b's ruling established is *not* the problem — the problem is an
/// *admitted* one. So the control plane either takes the work now or refuses it, and the refusal
/// costs no blocking thread, no queue slot and no WAL byte.
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

/// Process-wide server state, shared (behind `Arc`) across every axum handler on every plane.
pub struct AppState {
    pub engine: Engine,
    pub sessions: Mutex<SessionRegistry>,
    pub max_k: usize,
    /// D-B: the viewer/session admission gate. Never touched by the control plane (D13).
    pub compute_gate: ComputeGate,
    /// Task 6 (D2): the control plane's own admission bound. Deliberately **not** `compute_gate` —
    /// D13 keeps the control plane out of the viewer gate, because an ingest batch durability-
    /// syncing must not be throttled by the budget a slow viewport consumes.
    pub ingest_admission: IngestAdmission,
    /// Task 6 (D1): per-request row cap on `/control/ingest`; over is 422. Checked after the Arrow
    /// decode, which is the earliest point the row count is knowable.
    pub ingest_max_batch_rows: usize,
    /// Task 6 (D1): per-request body-byte cap on `/control/ingest`; over is 422.
    ///
    /// **Enforced by a `DefaultBodyLimit` layer on the route, not by a length check in the
    /// handler** — see `control::router`. Carried here so the layer and the 422's detail string
    /// read the same number.
    pub ingest_max_batch_bytes: usize,
    /// Runtime half of the `x-tessera-stage-ns` gate (see `Config::stage_timing`). The other half
    /// is the `bench-timing` compile feature; both must hold.
    pub stage_timing: bool,
    /// Parsed and stored (design §7.5/§2.3's startup rule); not consumed by any Phase 1 handler.
    #[allow(dead_code)]
    pub min_visible_members: u64,
    pub session_credential: String,
    pub operator_credential: String,
    /// `serve.dev_cors_origins`. Empty — the default — means the viewer and session routers mount
    /// no CORS layer at all. See [`crate::cors`] for why this is a development affordance and why
    /// the control plane never consults it.
    pub dev_cors_origins: Vec<String>,
}

impl AppState {
    /// Bearer-token lookup for the viewer plane: an unrecognised token is `bad-credential` (401);
    /// a recognised-but-expired one is `expired-token` (403) — the engine itself never checks
    /// `expires_at` (Task 11's report flags this as a server obligation this method exists to
    /// discharge).
    pub fn authenticated_session(
        &self,
        token: &str,
    ) -> Result<std::sync::Arc<SessionEntry>, ApiError> {
        let entry = self
            .sessions
            .lock()
            .get(token)
            .ok_or(ApiError::BadCredential)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs();
        if now >= entry.session.expires_at {
            return Err(ApiError::ExpiredToken);
        }
        Ok(entry)
    }

    /// Bearer check for the session and control planes' shared-secret credentials, in time
    /// independent of *where* a wrong guess diverges.
    ///
    /// **The previous implementation was `token == expected`, and its doc called that
    /// "constant-time-ish". That claim was false** (Task 5): `str` equality short-circuits on the
    /// first differing byte *and* on a length mismatch, which is a prefix oracle over the operator
    /// and session credentials — an attacker who can time this recovers the secret byte by byte in
    /// linear rather than exponential guesses. The scoping excuse ("Phase 1 does not harden against
    /// timing side channels") did not survive contact with the fact that these two secrets are the
    /// whole of the admin and session planes' authentication.
    ///
    /// **How this is fixed, and what it still does not claim.** Both sides are hashed to 32 bytes
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
mod compute_gate_tests {
    use super::*;

    /// D-B stage 1: with `compute_admission = 1, compute_queue = 0` (the deterministic
    /// configuration this task's brief names), a second concurrent `admit()` while the first
    /// permit is still held sheds via `try_acquire` — no waiting, no timeout elapsed.
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

    /// D-B stage 2: a slot is available (queue has room) but the compute semaphore is fully
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

    /// No permit leak (spec constraint): after a shed, both the slot and compute permits the
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

    /// `/control/status`'s gauges (D-B): `in_flight` and `waiting` are derived from the
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
