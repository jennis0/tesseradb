//! **The orderings the artifact write paths are written against, constructed rather than argued.**
//!
//! The publications, the growths and the registrations take the work lane and are applied in
//! `Executor::run_work_pass`'s else-arm — own append, own fsync, own apply — **while a commit
//! window holding earlier-arriving ingest is still open**. So WAL append order stops equalling
//! submission order, and a publication executing there claims ordinals from a level's cursor
//! against a window that has not closed. Everything in this file is a consequence of that one
//! sentence: each case is an ordering that arm makes possible, and none of them had a test.
//!
//! ## How an ordering is made a sequence
//!
//! Almost none of this needs a race. The work lane is FIFO and one pass drains it whole, so if
//! two commands are queued in a known order before the executor looks, the pass that drains them
//! is deterministic. [`park`] is what buys that: a gate batch stalls the executor at
//! [`PauseSite::AfterFsync`] with the queue already empty, every command submitted from then until
//! the release stays queued, and `ExecutorStats::work_depth` says when each one has landed. The
//! ordering under test is then a sequence of calls, and reads as one.
//!
//! Three shapes are assembled differently and say so at their own doc: the fold, whose passes run
//! on their own thread and which is parked at [`PauseSite::BeforeCurrentFlip`] instead; the
//! atomicity pair, which parks the window under test rather than a gate in front of it; and the
//! reader case, whose whole subject is a genuine race.
//!
//! ## What is not constructible here, and why it is absent rather than weakened
//!
//! `Executor::close_window` applies the row swap and then the artifact records, with no pause site
//! between them, so **the crash that lands between a batch's rows and its joins cannot be reached
//! in-process**. What the atomicity case pins instead is the pair of states that *are* reachable —
//! both halves durable and neither in force, and a window abandoned before its fsync with neither
//! half anywhere — which is the same claim from both ends. A site between the two applies would
//! turn it into a direct observation; adding one is a change to `write.rs` and is not this file's.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine, EngineConfig};
use tessera_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::{
    BatchArtifacts, BatchMembership, IncomingArtifact, IncomingGrowth, UnallocatedRow,
};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ValueSet,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const LAYER: &str = "clusters/a";
/// Generous on purpose: every wait here is on a counter the executor moves, so a timeout is a hang
/// and never a slow machine — which makes a bound sized against a loaded box cost nothing.
const WAIT: Duration = Duration::from_secs(60);

// -------------------------------------------------------------------------------------------
// The fixture and its declarations
// -------------------------------------------------------------------------------------------

/// **No existence criterion and no supplied content**, which is what keeps these cases about
/// ordering: a criterion would turn a count this file cares about into an absence, and a supplied
/// kind would make every minting case a refusal at admission rather than an interleaving.
fn declaration(name: &str, value_set: ValueSet) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set,
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

impl Fixture {
    /// An engine whose executor runs against a switchboard this test holds the other end of, with
    /// the tick held long so the test owns the clock — a flush firing mid-assembly would publish
    /// against a state the case is still building.
    fn open_with_faults(&self) -> (Engine, Arc<FaultSwitchboard>) {
        let mut engine = Engine::open(
            &self.root,
            &self.cache,
            &self.wal,
            tessera_plugin::Passthrough::new(),
            EngineConfig {
                flush_max_age_secs: 3600,
                ..config_uncapped()
            },
        )
        .expect("engine should open against a freshly built bundle");
        let faults = Arc::new(FaultSwitchboard::new());
        engine
            .start_write_executor_with_faults(8, Arc::clone(&faults))
            .expect("the executor starts once");
        engine.set_background_refresh_for_test(false);
        (engine, faults)
    }

    /// The same engine with no switchboard — for the cases whose ordering comes from a thread or
    /// from `Engine`'s own fold controls rather than from a pause site.
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The corpus entities behind a run of source ids.
    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }
}

// -------------------------------------------------------------------------------------------
// The harness: parking the executor with its queue empty
// -------------------------------------------------------------------------------------------

/// An executor stalled mid-close, and the work-lane depth at the moment it stalled.
struct Parked<'scope> {
    gate: std::thread::ScopedJoinHandle<'scope, ()>,
    /// The gate's own entry is still counted here — it is completed at the end of the close the
    /// executor is parked inside — so every wait below is expressed as a delta from it.
    base: u64,
}

/// **Park the executor mid-close with its work queue empty**, which is what turns the orderings
/// below into sequences.
///
/// A gate batch of one row is submitted from its own thread; the executor drains it, closes the
/// window and stalls at [`PauseSite::AfterFsync`]. From the arrival until the release the executor
/// touches nothing, so a command submitted here is still queued when it runs again — and the work
/// lane is FIFO, so the order these cases submit in is the order `run_work_pass` drains.
///
/// The gate batch names no artifact and lands nowhere near the cases' geometry, so nothing it does
/// is visible to an assertion.
fn park<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    engine: &'env Engine,
    faults: &'env FaultSwitchboard,
) -> Parked<'scope> {
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let gate = scope.spawn(move || {
        engine
            .accept_ingest(
                vec![row(engine, "gate", 1.0, 1.0)],
                "gate".to_string(),
                body_hash("gate"),
            )
            .expect("the gate batch is an ordinary ingest");
    });
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);
    let base = engine.write_executor_stats().work_depth;
    Parked { gate, base }
}

/// Block until the work lane holds `depth` commands.
///
/// Called with the executor parked, where nothing completes, so this reads as "the submission has
/// reached the queue" — the observation that orders one submission after another instead of
/// betting on two threads.
fn wait_for_queue(engine: &Engine, depth: u64) {
    wait_until(&format!("the work lane reaches depth {depth}"), || {
        engine.write_executor_stats().work_depth >= depth
    });
}

/// Block until the deny lane has taken `n` submissions.
fn wait_for_denies(engine: &Engine, n: u64) {
    wait_until(&format!("the deny lane takes {n} submission(s)"), || {
        engine.write_executor_stats().deny_submitted >= n
    });
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

// -------------------------------------------------------------------------------------------
// Submitting, and reading back
// -------------------------------------------------------------------------------------------

fn body_hash(seed: &str) -> [u8; 32] {
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(seed.as_bytes()) {
        *slot = *byte;
    }
    hash
}

fn row(engine: &Engine, external_id: &str, x: f64, y: f64) -> UnallocatedRow {
    let descriptors = vec![b"0".to_vec()];
    UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        x,
        y,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        descriptors,
    }
}

/// One batch of one point carrying the artifact that point belongs to — what `/control/ingest`'s
/// membership column decodes to, taken at the engine boundary. Returns how many artifacts the
/// batch created.
fn ingest_naming(engine: &Engine, batch: &str, layer: &str, key: &str, x: f64, y: f64) -> u64 {
    engine
        .accept_ingest_joining(
            vec![row(engine, batch, x, y)],
            batch.to_string(),
            body_hash(batch),
            BatchArtifacts {
                memberships: vec![BatchMembership {
                    layer: layer.to_string(),
                    level: 0,
                    key: key.to_string(),
                    rows: vec![0],
                }],
                edges: Vec::new(),
            },
        )
        .expect("a point naming an artifact is an ordinary write")
        .1
}

fn artifacts_of(engine: &Engine) -> Vec<ArtifactOut> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// One artifact's masked count for a principal who can see everything, so the number is the
/// membership's own size. `None` where the artifact is not served at all.
fn count_of(engine: &Engine, key: &str) -> Option<u64> {
    artifacts_of(engine)
        .into_iter()
        .find(|a| a.key.as_deref() == Some(key))
        .map(|a| a.masked_count)
}

fn artifact_entity(engine: &Engine, id: tessera_types::TesseraId) -> EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0].expect("it names what was issued")
}

/// Request a flush and block until it has published — what gives an ingested point a base row, and
/// therefore what makes it count towards any membership it joined.
fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush publishes", || {
        engine.write_executor_stats().flushes > before
    });
}

/// Request a fold and block until it has published, asserting it was not discarded.
fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    wait_until("the fold publishes", || {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        now.folds > before.folds
    });
}

/// **Flush, then fold** — what an *ingested* point needs before it counts towards a membership it
/// joined, and what `artifact_growth.rs` does for the same reason.
///
/// Measured rather than reasoned: a case that flushed and stopped read the artifact at the size it
/// had before the batch, and a case whose members are corpus points needs neither, their rows being
/// in the bundle already. Which of the two publications carries the projection is not asserted here
/// — only that a batch's own point is not countable until both have run.
fn settle(engine: &Engine) {
    flush(engine);
    fold(engine);
}

/// Ask for a flush against an empty buffer: nothing to flush, so the tick rotates the log rather
/// than publishing geometry — the one place the growth pin bites.
fn rotate(engine: &Engine) {
    let before = engine.write_executor_stats().ticks;
    engine.request_flush();
    wait_until("the tick that rotates the log runs", || {
        engine.write_executor_stats().ticks > before
    });
}

/// The log's surviving members, oldest first.
fn wal_members(fx: &Fixture) -> Vec<String> {
    let dir = fx.wal.parent().expect("the log has a directory");
    let stem = fx
        .wal
        .file_stem()
        .expect("the log has a stem")
        .to_string_lossy()
        .to_string();
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(&format!("{stem}-")) && n.ends_with(".log"))
        .collect();
    found.sort();
    found
}

fn remove_the_whole_log(fx: &Fixture) {
    let dir = fx.wal.parent().expect("the log has a directory");
    let stem = fx.wal.file_stem().expect("the log has a stem").to_owned();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{}-", stem.to_string_lossy())) {
            std::fs::remove_file(entry.path()).expect("a log member is removable");
            removed += 1;
        }
    }
    assert!(
        removed > 0,
        "no log member was found to delete — the test would prove nothing"
    );
}

/// The live prefix's membership extents — the durable name count a publication's pack increments.
fn membership_files(fx: &Fixture, engine: &Engine) -> usize {
    let dir = fx
        .root
        .join(&engine.generation().prefix)
        .join("partitions")
        .join("default")
        .join("members");
    std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "tsmb"))
        .count()
}

/// Publish one artifact and block until it is durable in an extent, which is what puts it **below**
/// its level's published high-water — the mark an append-only tail pack starts from, and therefore
/// what makes a later growth a record only the fold's whole rewrite reaches.
fn publish_and_pack(fx: &Fixture, engine: &Engine, key: &str, members: Vec<EntityId>) {
    let before = membership_files(fx, engine);
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::from_entities(Some(key.into()), members)],
        )
        .expect("a publication into a registered layer");
    wait_until("the publication reaches an extent", || {
        membership_files(fx, engine) > before
    });
}

// -------------------------------------------------------------------------------------------
// 1. A publication inside the window a batch is waiting in
// -------------------------------------------------------------------------------------------

/// **A key that acquired an artifact between a batch's admission and its window's close grows
/// rather than mints.** The batch was admitted while nothing held `k`, so its membership carried no
/// ordinal; the publication behind it in the queue executed in the else-arm — own append, own
/// fsync — and claimed the ordinal from the level's cursor with the window still open. The close
/// re-resolves against `ArtifactStore::ordinal_of_key` and finds it.
///
/// Minting at admission instead is what this refuses: the batch and the publication would each
/// have claimed the same cursor, and the level would hold two artifacts under one key.
///
/// **The count the batch is answered with is what discriminates it.** A window closed before the
/// publication ran, an ordinal claimed at admission, or a publication deferred to the next pass all
/// produce a batch that reports one artifact created — and a level holding two under `k`.
#[test]
fn a_publication_that_lands_mid_window_binds_the_batch_that_named_its_key() {
    let fx = fixture();
    let (engine, faults) = fx.open_with_faults();
    engine
        .register_layer(declaration(LAYER, ValueSet::Open))
        .unwrap();
    let members = fx.members(0..300);

    let minted = std::thread::scope(|s| {
        let parked = park(s, &engine, &faults);
        let batch = s.spawn(|| ingest_naming(&engine, "b1", LAYER, "k", 5.0, 5.0));
        wait_for_queue(&engine, parked.base + 1);
        let publish = s.spawn(|| {
            engine
                .publish_artifacts(
                    LAYER.into(),
                    0,
                    vec![IncomingArtifact::from_entities(Some("k".into()), members)],
                )
                .expect("the publication executes while the window is open")
        });
        wait_for_queue(&engine, parked.base + 2);

        faults.release();
        parked.gate.join().unwrap();
        publish.join().unwrap();
        batch.join().unwrap()
    });

    assert_eq!(
        minted, 0,
        "the batch created nothing: its key named an artifact by the time the window closed"
    );
    assert_eq!(
        engine.published_artifacts(),
        1,
        "one key, one artifact — a mint claimed at admission would have made a second"
    );
    settle(&engine);
    assert_eq!(
        count_of(&engine, "k"),
        Some(301),
        "and the batch's point is in the artifact the publication made, not in one of its own"
    );
}

// -------------------------------------------------------------------------------------------
// 2. Two batches in one window naming one unminted key
// -------------------------------------------------------------------------------------------

/// **One artifact per key per level for the whole window.** Two batches drained into one window
/// both name `k`, which nothing holds; the mint is planned once over the window's gathered keys,
/// so one artifact is created and both batches' points are in it.
///
/// The fsync count is asserted because it is the only external evidence that the two batches were
/// in *one* window: one fsync per close, so two windows would be two.
#[test]
fn two_batches_in_one_window_naming_one_unminted_key_mint_it_once() {
    let fx = fixture();
    let (engine, faults) = fx.open_with_faults();
    engine
        .register_layer(declaration(LAYER, ValueSet::Open))
        .unwrap();

    let (fsyncs_before, first, second) = std::thread::scope(|s| {
        let parked = park(s, &engine, &faults);
        let fsyncs_before = engine.write_executor_stats().wal_fsyncs;
        let b1 = s.spawn(|| ingest_naming(&engine, "b1", LAYER, "k", 5.0, 5.0));
        wait_for_queue(&engine, parked.base + 1);
        let b2 = s.spawn(|| ingest_naming(&engine, "b2", LAYER, "k", 6.0, 6.0));
        wait_for_queue(&engine, parked.base + 2);

        faults.release();
        parked.gate.join().unwrap();
        (fsyncs_before, b1.join().unwrap(), b2.join().unwrap())
    });

    assert_eq!(
        engine.write_executor_stats().wal_fsyncs - fsyncs_before,
        1,
        "the two batches closed one window, so they paid one fsync between them"
    );
    assert_eq!(first, 1, "the first batch to name the key owns the mint");
    assert_eq!(second, 0, "and the second joins what the first made");
    assert_eq!(engine.published_artifacts(), 1);

    settle(&engine);
    assert_eq!(
        count_of(&engine, "k"),
        Some(2),
        "both batches' points are in the one artifact their key created"
    );
}

// -------------------------------------------------------------------------------------------
// 3. A growth and a window close, either way round
// -------------------------------------------------------------------------------------------

/// Which of the two commands the executor drains first.
#[derive(Debug, Clone, Copy)]
enum Order {
    /// The growth is queued behind the batch, so it executes in the else-arm with the batch's
    /// window open and the batch's join is applied after it.
    GrowthInsideTheWindow,
    /// The growth is queued in front, so it is applied before the batch is even admitted.
    GrowthBeforeTheWindow,
}

/// The membership `c0` ends with when a growth and a batch naming it are drained in `order` from
/// one work pass.
fn membership_when(order: Order) -> u64 {
    let fx = fixture();
    let (engine, faults) = fx.open_with_faults();
    engine
        .register_layer(declaration(LAYER, ValueSet::Open))
        .unwrap();
    publish_and_pack(&fx, &engine, "c0", fx.members(0..300));
    let joining = fx.members(300..310);

    std::thread::scope(|s| {
        let parked = park(s, &engine, &faults);
        let grow = || {
            engine
                .grow_memberships(
                    LAYER.into(),
                    0,
                    vec![IncomingGrowth::from_entities("c0".into(), joining)],
                )
                .expect("points joining an artifact that exists is an ordinary write")
        };
        let batch = || ingest_naming(&engine, "b1", LAYER, "c0", 5.0, 5.0);

        // Spawned in the order the work lane is to hold them, each one confirmed onto the queue
        // before the next is submitted.
        match order {
            Order::GrowthInsideTheWindow => {
                let b = s.spawn(batch);
                wait_for_queue(&engine, parked.base + 1);
                let g = s.spawn(grow);
                wait_for_queue(&engine, parked.base + 2);
                faults.release();
                parked.gate.join().unwrap();
                g.join().unwrap();
                assert_eq!(b.join().unwrap(), 0, "c0 exists, so nothing is minted");
            }
            Order::GrowthBeforeTheWindow => {
                let g = s.spawn(grow);
                wait_for_queue(&engine, parked.base + 1);
                let b = s.spawn(batch);
                wait_for_queue(&engine, parked.base + 2);
                faults.release();
                parked.gate.join().unwrap();
                g.join().unwrap();
                assert_eq!(b.join().unwrap(), 0, "c0 exists, so nothing is minted");
            }
        }
    });

    settle(&engine);
    count_of(&engine, "c0").expect("c0 is served")
}

/// **A growth and a window close commute.** The growth executes immediately whichever side of the
/// admission it arrives on — in front of the batch, or in the else-arm with the batch's window
/// still open — and the artifact holds the same members either way. There is no state in which a
/// cluster holds some of its points because of how they arrived.
#[test]
fn a_growth_racing_the_window_close_leaves_the_same_membership_either_way() {
    let inside = membership_when(Order::GrowthInsideTheWindow);
    let before = membership_when(Order::GrowthBeforeTheWindow);
    assert_eq!(
        inside, 311,
        "the published 300, the growth's 10 and the batch's own point"
    );
    assert_eq!(
        inside, before,
        "the drain order decided the WAL's order and nothing else"
    );
}

// -------------------------------------------------------------------------------------------
// 4. The fold, and the pin only it releases
// -------------------------------------------------------------------------------------------

/// **A growth that lands while the fold is parked at its flip is pinned by its own record, not by
/// the one the fold just packed.** `mark_growth_packed` runs just after the `CURRENT` rename and
/// clears the pin whole; a growth applied after it sets the pin again from its own position, and
/// rotation may not reclaim past that.
///
/// The failure this is written against is silent: the level is packed only above its published
/// high-water, so a growth below it that lost its log member is an artifact back at the size it
/// was, with nothing refused and nothing logged.
#[test]
fn a_growth_that_lands_after_the_folds_pack_pins_the_log_again() {
    let fx = fixture();
    {
        let (engine, faults) = fx.open_with_faults();
        engine
            .register_layer(declaration(LAYER, ValueSet::Open))
            .unwrap();
        publish_and_pack(&fx, &engine, "c0", fx.members(0..300));
        engine
            .grow_memberships(
                LAYER.into(),
                0,
                vec![IncomingGrowth::from_entities(
                    "c0".into(),
                    fx.members(300..310),
                )],
            )
            .unwrap();

        // Sealed into its own member before the fold, so that the two growths are in *different*
        // members and the assertion below is about the late one. Without this they share the
        // active member and the early growth's pin — which the fold is about to release — would
        // keep it alive whatever the late growth did.
        rotate(&engine);
        let early_member = wal_members(&fx)
            .into_iter()
            .rev()
            .nth(1)
            .expect("the rotation sealed a member behind the new active one");

        let late = fx.members(310..320);
        faults.arm_pause(PauseSite::BeforeCurrentFlip, PauseAction::Stall);
        engine.request_fold();
        faults.await_arrivals(PauseSite::BeforeCurrentFlip, 1, WAIT);

        // Queued behind a fold that has written its whole tree and named none of it. The growth is
        // applied only after the flip, and therefore after the pack that clears the pin.
        let before_fold = engine.write_executor_stats().folds;
        std::thread::scope(|s| {
            let grown = s.spawn(|| {
                engine
                    .grow_memberships(
                        LAYER.into(),
                        0,
                        vec![IncomingGrowth::from_entities("c0".into(), late)],
                    )
                    .expect("a growth behind a parked fold is an ordinary write")
            });
            faults.release();
            wait_until("the released fold publishes", || {
                engine.write_executor_stats().folds > before_fold
            });
            grown.join().unwrap();
        });

        assert_eq!(
            count_of(&engine, "c0"),
            Some(320),
            "both growths are in force"
        );
        let late_member = wal_members(&fx)
            .pop()
            .expect("the log has at least one member");
        assert_ne!(
            late_member, early_member,
            "the two growths must be in different members or this proves nothing"
        );
        // Two rotations: the first seals the member the late growth is in, the second is the one
        // that would find it sealed and reclaim it.
        rotate(&engine);
        rotate(&engine);
        let members = wal_members(&fx);
        assert!(
            !members.contains(&early_member),
            "{early_member} holds only the growth the fold's rewrite packed, so \
             `mark_growth_packed` released it and rotation reclaimed it. Members now: {members:?}"
        );
        assert!(
            members.contains(&late_member),
            "{late_member} holds a growth the pack did not cover, and the pin that growth set \
             *after* `mark_growth_packed` is what keeps rotation off it. Members now: {members:?}"
        );
    }

    let engine = fx.open();
    assert_eq!(
        count_of(&engine, "c0"),
        Some(320),
        "the growth that landed after the pack replayed from the member the pin saved"
    );
}

/// **The same membership comes back whether the log is replayed or the prefix is read.** Before the
/// fold, the growth's only home is the log — and a tail publication that packs above it and marks
/// the level published does not release it. After the fold there is no log left to read.
#[test]
fn the_membership_replays_the_same_on_either_side_of_the_folds_pack() {
    let fx = fixture();
    let before_the_pack = {
        let engine = fx.open();
        engine
            .register_layer(declaration(LAYER, ValueSet::Open))
            .unwrap();
        publish_and_pack(&fx, &engine, "c0", fx.members(0..300));
        engine
            .grow_memberships(
                LAYER.into(),
                0,
                vec![IncomingGrowth::from_entities(
                    "c0".into(),
                    fx.members(300..310),
                )],
            )
            .unwrap();
        // A tail publication: its extent is packed above the high-water the grown record sits
        // below, and its publication marks the level published to the top.
        publish_and_pack(&fx, &engine, "c1", fx.members(400..410));
        rotate(&engine);
        rotate(&engine);
        drop(engine);

        let engine = fx.open();
        count_of(&engine, "c0").expect("c0 is served")
    };
    assert_eq!(
        before_the_pack, 310,
        "the growth survived a tail publication, two rotations and a restart on the log alone"
    );

    let engine = fx.open();
    fold(&engine);
    drop(engine);
    remove_the_whole_log(&fx);

    let engine = fx.open();
    assert_eq!(
        count_of(&engine, "c0"),
        Some(before_the_pack),
        "and reads back identically from the prefix the fold wrote, with no log to replay"
    );
}

// -------------------------------------------------------------------------------------------
// 5. A suppression against a publication into what it hides
// -------------------------------------------------------------------------------------------

/// **A suppression and a batch joining the artifact it hides, in one drain.** The batch is
/// submitted first and applied second: the two take different lanes and `Executor::run` visits the
/// deny lane before the work pass, so the suppression is in force by the time the batch is
/// admitted. The join lands anyway, because a growth resolves its key against the *store* and never
/// against what is served.
///
/// The fail-closed outcome is that the artifact stays out of every viewport across the join and
/// across a fold: a suppression retires only on unsuppress (write-path §5.4, Rule S), and a fold
/// is not one. What the unsuppress then reveals is the membership the join gave it — the points
/// joined exactly as they would have otherwise.
#[test]
fn a_suppression_racing_a_join_hides_the_artifact_and_keeps_the_join() {
    let fx = fixture();
    let (engine, faults) = fx.open_with_faults();
    engine
        .register_layer(declaration(LAYER, ValueSet::Open))
        .unwrap();
    let id = engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(0..300),
            )],
        )
        .unwrap()[0];
    wait_until("the publication lands", || {
        engine.published_artifacts() == 1
    });
    let entity = artifact_entity(&engine, id);

    std::thread::scope(|s| {
        let parked = park(s, &engine, &faults);
        let denies_before = engine.write_executor_stats().deny_submitted;
        let batch = s.spawn(|| ingest_naming(&engine, "b1", LAYER, "c0", 5.0, 5.0));
        wait_for_queue(&engine, parked.base + 1);
        let suppress = s.spawn(|| {
            engine
                .accept_change(entity, ChangeOp::Suppress)
                .expect("a suppression is accepted")
        });
        wait_for_denies(&engine, denies_before + 1);

        faults.release();
        parked.gate.join().unwrap();
        suppress.join().unwrap();
        assert_eq!(batch.join().unwrap(), 0, "c0 exists, so nothing is minted");
    });

    flush(&engine);
    assert_eq!(
        count_of(&engine, "c0"),
        None,
        "the suppression is in force against a batch that joined it in the same drain"
    );
    fold(&engine);
    assert_eq!(
        count_of(&engine, "c0"),
        None,
        "and a fold is not a retirement route for one"
    );

    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert_eq!(
        count_of(&engine, "c0"),
        Some(301),
        "the point joined while it was hidden, exactly as it would have otherwise"
    );
}

/// **A layer's own suppression covers a publication that landed in the same drain.** The layer is
/// gated per request, not at the publication, so an artifact published into a layer suppressed a
/// moment earlier is stored and served to nobody — including the one this drain created.
///
/// The unsuppress is what says the publication happened at all: both artifacts appear, so the
/// refusal was a gate and not a lost write.
#[test]
fn a_layers_suppression_covers_a_publication_that_landed_beside_it() {
    let fx = fixture();
    let (engine, faults) = fx.open_with_faults();
    let layer_id = engine
        .register_layer(declaration(LAYER, ValueSet::Open))
        .unwrap();
    let layer_entity = artifact_entity(&engine, layer_id);
    publish_and_pack(&fx, &engine, "c0", fx.members(0..300));

    let late = fx.members(400..410);
    std::thread::scope(|s| {
        let parked = park(s, &engine, &faults);
        let denies_before = engine.write_executor_stats().deny_submitted;
        let publish = s.spawn(|| {
            engine
                .publish_artifacts(
                    LAYER.into(),
                    0,
                    vec![IncomingArtifact::from_entities(Some("c1".into()), late)],
                )
                .expect("a publication into a layer is not gated on the layer's own verdict")
        });
        wait_for_queue(&engine, parked.base + 1);
        let suppress = s.spawn(|| {
            engine
                .accept_change(layer_entity, ChangeOp::Suppress)
                .expect("a layer's suppression is accepted")
        });
        wait_for_denies(&engine, denies_before + 1);

        faults.release();
        parked.gate.join().unwrap();
        suppress.join().unwrap();
        publish.join().unwrap();
    });

    assert_eq!(
        engine.published_artifacts(),
        2,
        "both artifacts are in the store"
    );
    assert!(
        artifacts_of(&engine).is_empty(),
        "and neither is served: the layer's suppression covers the publication beside it"
    );
    fold(&engine);
    assert!(
        artifacts_of(&engine).is_empty(),
        "a fold retires deletions, never suppressions"
    );

    engine
        .accept_change(layer_entity, ChangeOp::Unsuppress)
        .unwrap();
    let keys: Vec<String> = artifacts_of(&engine)
        .into_iter()
        .filter_map(|a| a.key)
        .collect();
    let mut keys = keys;
    keys.sort();
    assert_eq!(
        keys,
        vec!["c0".to_string(), "c1".to_string()],
        "the publication was gated, not lost"
    );
}

// -------------------------------------------------------------------------------------------
// 6. The window's two halves, together or not at all
// -------------------------------------------------------------------------------------------

/// **Durable, and not in force.** Parked after the window's one fsync, the batch's rows and the
/// record that mints its artifact are both on disc and neither is applied: the point resolves to
/// nothing and the level holds no artifact. The release applies both, and a restart that reads
/// nothing but the log gets both.
///
/// There is no reachable state between them — the swap and the artifact apply are consecutive
/// statements with no site in between — so what this pins is the pair of endpoints.
#[test]
fn a_batchs_rows_and_its_joins_are_durable_together_and_applied_together() {
    let fx = fixture();
    {
        let (engine, faults) = fx.open_with_faults();
        engine
            .register_layer(declaration(LAYER, ValueSet::Open))
            .unwrap();

        faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
        std::thread::scope(|s| {
            let batch = s.spawn(|| ingest_naming(&engine, "b1", LAYER, "k", 5.0, 5.0));
            faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);

            assert_eq!(
                engine.published_artifacts(),
                0,
                "the mint record is durable and the level does not hold it yet"
            );
            assert!(
                engine
                    .resolve_external_id(b"b1")
                    .expect("the lookup answers")
                    .is_none(),
                "and the row it was appended beside is not in force either"
            );

            faults.release();
            assert_eq!(batch.join().unwrap(), 1, "the key created its artifact");
        });
        assert_eq!(engine.published_artifacts(), 1);
        // Deliberately not flushed: the rows and the mint are in the log and nowhere else, which is
        // what the restart below has to read.
    }

    let engine = fx.open();
    assert_eq!(
        engine.published_artifacts(),
        1,
        "the mint replayed from the log"
    );
    assert!(
        engine
            .resolve_external_id(b"b1")
            .expect("the lookup answers")
            .is_some(),
        "and so did the row that named it — one fsync covered both"
    );
    settle(&engine);
    assert_eq!(
        count_of(&engine, "k"),
        Some(1),
        "the point is in the artifact it created"
    );
}

/// **A window abandoned before its fsync leaves neither half anywhere.** The append fails, so every
/// waiter is refused, nothing is applied, and a restart reading the log finds no row and no
/// artifact — not the state the atomicity claim forbids, where a point is ingested and its
/// membership is absent.
///
/// An injected failure is indistinguishable from a real one in variant and in order
/// (`tessera_lifecycle::faults`'s fidelity rule), so this is the disk-full a real close would meet.
#[test]
fn a_window_that_could_not_append_leaves_neither_the_rows_nor_the_joins() {
    let fx = fixture();
    {
        let (engine, faults) = fx.open_with_faults();
        engine
            .register_layer(declaration(LAYER, ValueSet::Open))
            .unwrap();

        faults.fail_next_appends(1);
        let refused = engine
            .accept_ingest_joining(
                vec![row(&engine, "b1", 5.0, 5.0)],
                "b1".to_string(),
                body_hash("b1"),
                BatchArtifacts {
                    memberships: vec![BatchMembership {
                        layer: LAYER.to_string(),
                        level: 0,
                        key: "k".to_string(),
                        rows: vec![0],
                    }],
                    edges: Vec::new(),
                },
            )
            .expect_err("the window's append failed, so the batch is refused");
        assert_eq!(
            engine.write_executor_posture(),
            tessera_engine::ExecutorPosture::WalPoisoned,
            "the refusal was the durability failure it was, not a check further up: {refused}"
        );
        assert_eq!(
            engine.published_artifacts(),
            0,
            "no artifact was minted for a window that never became durable"
        );
    }

    let engine = fx.open();
    assert_eq!(
        engine.published_artifacts(),
        0,
        "and none replayed: the record was never written"
    );
    assert!(
        engine
            .resolve_external_id(b"b1")
            .expect("the lookup answers")
            .is_none(),
        "nor did the row, which is the half that would otherwise be a point with no membership"
    );
}

// -------------------------------------------------------------------------------------------
// 7. A reader against a writer
// -------------------------------------------------------------------------------------------

/// **A reader never sees a membership part-grown.** One thread serves the viewport while another
/// ingests, publishes and grows; every count the reader takes is one of the values a completed
/// growth leaves, never a value between two of them, and it never goes backwards.
///
/// The only genuinely racing case in this file, and bounded rather than timed: a fixed number of
/// rounds, each a whole growth of ten, so a torn read is a count that is not a multiple of ten
/// above the published 300 rather than something to be judged by eye.
#[test]
fn a_reader_sees_the_membership_move_forward_through_whole_growths_only() {
    const ROUNDS: u64 = 12;
    const STEP: u64 = 10;

    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(LAYER, ValueSet::Open))
        .unwrap();
    publish_and_pack(&fx, &engine, "c0", fx.members(0..300));

    // Resolved up front: the map is read off the bundle, and doing it inside the writer's loop
    // would make the loop's cost the fixture's rather than the write path's.
    let joining: Vec<Vec<EntityId>> = (0..ROUNDS)
        .map(|i| fx.members(300 + i * STEP..300 + (i + 1) * STEP))
        .collect();
    let extra = fx.members(0..10);

    let stop = AtomicBool::new(false);
    let observed: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        s.spawn(|| {
            let deadline = Instant::now() + WAIT;
            let mut last = (0u64, 0usize);
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let served = artifacts_of(&engine);
                let count = served
                    .iter()
                    .find(|a| a.key.as_deref() == Some("c0"))
                    .map(|a| a.masked_count)
                    .expect("c0 is served throughout");
                assert!(
                    (300..=300 + ROUNDS * STEP).contains(&count),
                    "a count outside every state this workload passes through: {count}"
                );
                assert_eq!(
                    (count - 300) % STEP,
                    0,
                    "a growth was read half-applied: {count}"
                );
                assert!(
                    count >= last.0 && served.len() >= last.1,
                    "freshness went backwards: {:?} then {:?}",
                    last,
                    (count, served.len())
                );
                last = (count, served.len());
                observed.lock().unwrap().push(last);
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        for (i, members) in joining.into_iter().enumerate() {
            engine
                .grow_memberships(
                    LAYER.into(),
                    0,
                    vec![IncomingGrowth::from_entities("c0".into(), members)],
                )
                .expect("a growth against a served artifact");
            engine
                .accept_ingest(
                    vec![row(&engine, &format!("stress-{i}"), 5.0, 5.0)],
                    format!("stress-{i}"),
                    body_hash(&format!("stress-{i}")),
                )
                .expect("an ingest beside the growth");
            engine
                .publish_artifacts(
                    LAYER.into(),
                    0,
                    vec![IncomingArtifact::from_entities(
                        Some(format!("c{}", i + 1)),
                        extra.clone(),
                    )],
                )
                .expect("a publication beside the growth");
            // **The reader must actually overlap the writer or its assertions never run**, and a
            // round is three windows on an idle executor — fast enough that a loaded box could
            // finish the whole loop inside one viewport. A pause per round is what makes the
            // overlap a property of the test rather than of the scheduler.
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
    });

    assert_eq!(
        count_of(&engine, "c0"),
        Some(300 + ROUNDS * STEP),
        "every growth landed"
    );
    let observed = observed.into_inner().unwrap();
    let distinct: std::collections::BTreeSet<_> = observed.iter().collect();
    assert!(
        distinct.len() >= 3,
        "the reader barely overlapped the writer, so it asserted almost nothing: {} sample(s) \
         over {} distinct state(s)",
        observed.len(),
        distinct.len()
    );
}

// -------------------------------------------------------------------------------------------
// 8. The same orderings against a bundle that was built rather than written
// -------------------------------------------------------------------------------------------

/// One open clustering, built into the bundle with its roster and its members — the build-plane
/// half of [`declaration`].
const BUILT_CONFIG: &str = r#"
[sources]
clusters         = "clusters.parquet"
clusters_members = "clusters_members.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
source = "clusters"
membership = "enumerated"
value_set = "open"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members"
"#;

/// The corpus source ids the built cluster holds.
const BUILT_MEMBERS: std::ops::Range<u64> = 0..150;

fn write_parquet(
    path: &std::path::Path,
    schema: Arc<arrow::datatypes::Schema>,
    batch: arrow::record_batch::RecordBatch,
) {
    let mut w =
        parquet::arrow::ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
            .unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// A bundle built with its layer and its artifact — no control-plane call anywhere.
fn built_fixture() -> Fixture {
    use arrow::array::{ArrayRef, StringArray, UInt32Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points_n(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);

    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, BUILT_CONFIG).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture config parses");

    let clusters_schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    write_parquet(
        &tmp.path().join("clusters.parquet"),
        clusters_schema.clone(),
        RecordBatch::try_new(
            clusters_schema,
            vec![Arc::new(StringArray::from(vec!["c-built"])) as ArrayRef],
        )
        .unwrap(),
    );

    let members_schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("rank", DataType::UInt32, true),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let rows: Vec<u64> = BUILT_MEMBERS.collect();
    write_parquet(
        &tmp.path().join("clusters_members.parquet"),
        members_schema.clone(),
        RecordBatch::try_new(
            members_schema,
            vec![
                Arc::new(StringArray::from(vec!["c-built"; rows.len()])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![None::<u32>; rows.len()])),
                Arc::new(UInt64Array::from(rows.clone())),
            ],
        )
        .unwrap(),
    );

    tessera_build::build(&tessera_build::BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: root.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a build carrying its layer");

    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

/// **A built bundle takes the same interleaving, and reissues nothing** ([decision
/// 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)). The layer and
/// its artifact came from the build rather than from a registration, and an ingest batch whose key
/// is claimed by an online publication mid-window binds to that publication exactly as it does
/// against a bundle written entirely through the control plane.
///
/// The ids are the half a build can get wrong on its own: the row-less region grows downward from
/// where the build left it, so an online artifact taking an id the build spent would be two
/// entities under one `tessera_id`.
#[test]
fn a_built_bundle_takes_the_mid_window_publication_without_reissuing_an_id() {
    let fx = built_fixture();
    let (engine, faults) = fx.open_with_faults();
    let built: Vec<EntityId> = artifacts_of(&engine)
        .iter()
        .map(|a| artifact_entity(&engine, a.tessera_id))
        .collect();
    assert_eq!(built.len(), 1, "the build published one artifact");
    let members = fx.members(200..250);

    let minted = std::thread::scope(|s| {
        let parked = park(s, &engine, &faults);
        let batch = s.spawn(|| ingest_naming(&engine, "b1", LAYER, "k-online", 5.0, 5.0));
        wait_for_queue(&engine, parked.base + 1);
        let publish = s.spawn(|| {
            engine
                .publish_artifacts(
                    LAYER.into(),
                    0,
                    vec![IncomingArtifact::from_entities(
                        Some("k-online".into()),
                        members,
                    )],
                )
                .expect("a built layer takes a publication")
        });
        wait_for_queue(&engine, parked.base + 2);

        faults.release();
        parked.gate.join().unwrap();
        publish.join().unwrap();
        batch.join().unwrap()
    });

    assert_eq!(minted, 0, "the publication claimed the key first");
    assert_eq!(
        engine.published_artifacts(),
        2,
        "the built artifact and the online one — not three"
    );
    settle(&engine);
    assert_eq!(
        count_of(&engine, "k-online"),
        Some(51),
        "the online publication's members and the batch's own point"
    );
    assert_eq!(
        count_of(&engine, "c-built"),
        Some(BUILT_MEMBERS.count() as u64),
        "and the built artifact is untouched by any of it"
    );

    let online = artifacts_of(&engine)
        .iter()
        .find(|a| a.key.as_deref() == Some("k-online"))
        .map(|a| artifact_entity(&engine, a.tessera_id))
        .expect("the online artifact is served");
    assert!(
        !built.contains(&online),
        "an entity the build spent was handed out a second time"
    );
    assert!(
        built.iter().all(|e| online.raw() > e.raw()),
        "the artifact region continued above where the build left it rather than restarting \
         inside it: online {} built {:?}",
        online.raw(),
        built.iter().map(|e| e.raw()).collect::<Vec<_>>()
    );
}
