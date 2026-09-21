//! **Growth**: entities joining an artifact that already exists, and coming back after a restart.
//!
//! A build reading a member table has always entered points into an enumerated membership, so this
//! is that operation at the other entry point ([decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
//! What is new is that state now enters the artifact store *without* a whole record, which puts one
//! failure at the centre of this file and everything else around it.
//!
//! **The failure is silent.** A level is packed only above its published high-water, so a record
//! that grew below that mark never reaches a manifest again: durable in the log, absent from every
//! extent, and — once the log is reclaimed — back to its pre-growth membership at the next restart.
//! The artifact then serves the count it had before, which a viewer cannot tell from an artifact
//! that failed its existence criterion. So every case here that matters ends in a restart, and the
//! decisive one deletes the log outright: whatever comes back after that came from the prefix.
//!
//! **Minting is at the end of the file and is the same question one step further on**: an ingest
//! batch's key that no artifact holds *creates* the artifact, at the commit window's close and
//! inside its fsync, on a layer whose `value_set` is open — a path no publication took before, whose
//! failure would be the same silence.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::membership::IncomingContent;
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
};
use tessera_types::EntityId;

/// **No existence criterion**, deliberately, as in the fold's own cases: these assertions are about
/// what a membership *is*, and a criterion would turn a wrong count into an absence — which is the
/// weaker assertion of the two and the one this file exists to distinguish from a real absence.
fn declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
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
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The same engine with a one-second tick period, for the one case here whose subject is the
    /// WAL gauge: that gauge is sampled at most once per period, so at the shipped 90 s a test
    /// reading it after each of three events would sit out four and a half minutes.
    fn open_with_short_tick(&self) -> Engine {
        let engine = open_engine_publishing_with_tick_period(&self.root, &self.cache, &self.wal, 1);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The corpus entities behind a run of source ids — what a clustering pipeline, or an ingest
    /// carrying a cluster id, would resolve its members to.
    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        self.members_of(source_ids)
    }

    /// The same, over any run of source ids — what a generating set is drawn from, which is a
    /// selection of the corpus rather than a contiguous range.
    fn members_of(&self, source_ids: impl Iterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }

    fn live_prefix(&self, engine: &Engine) -> std::path::PathBuf {
        self.root.join(&engine.generation().prefix)
    }

    fn membership_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        let dir = self
            .live_prefix(engine)
            .join("partitions")
            .join("default")
            .join("members");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tsmb"))
            .collect()
    }
}

/// The one artifact's masked count, for a principal who can see everything — so the number is the
/// membership's own size, and anything that moves it is the growth.
fn count(engine: &Engine) -> u64 {
    let artifacts = artifacts_of(engine, &full_coverage_credential());
    assert_eq!(
        artifacts.len(),
        1,
        "these cases publish exactly one artifact"
    );
    artifacts[0].masked_count
}

/// Wait until the executor has packed and marked published everything published so far — which is
/// what puts the artifact *below* its level's high-water and makes a later growth the case this
/// file is about.
fn wait_for_publication(fx: &Fixture, engine: &Engine, files: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fx.membership_files(engine).len() < files {
        assert!(
            std::time::Instant::now() < deadline,
            "the membership extents were never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Publish one artifact over `sources` and wait for it to be durable in an extent.
fn publish(
    fx: &Fixture,
    engine: &Engine,
    sources: std::ops::Range<u64>,
) -> tessera_types::TesseraId {
    engine.register_layer(declaration("clusters/a")).unwrap();
    let ids = engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources),
            )],
        )
        .unwrap();
    wait_for_publication(fx, engine, 1);
    ids[0]
}

/// Points join `c0`, and the tick that publishes the join into the level's row forms.
///
/// **The two are separate moments** (`ingest.md` §1.3): the acknowledgement means durable and the
/// join reaches what a viewer is served at the next publication. Every assertion below is about
/// what is served, so the tick is here rather than in each test.
fn grow(fx: &Fixture, engine: &Engine, sources: std::ops::Range<u64>) {
    engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![IncomingGrowth::from_entities(
                "c0".into(),
                fx.members(sources),
            )],
        )
        .expect("points joining an artifact that exists is an ordinary write");
    tick(engine);
}

/// Ask for a flush against an empty buffer, which is the deny-only regime's rotation: nothing to
/// flush, so the tick rotates the log rather than publishing geometry.
fn rotate(engine: &Engine) {
    let before = engine.write_executor_stats().ticks;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().ticks == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the tick that rotates the log never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Tick until the WAL gauge has been walked again, and return the reading.
///
/// The sample is rate-limited to one per tick period (`Executor::sample_wal_gauge`), so a tick is
/// not enough on its own: `wal.samples` is what says a fresh walk has run. With the one-second
/// period `Fixture::open_with_short_tick` sets, this returns within about a second.
fn resampled_gauge(engine: &Engine) -> tessera_engine::WalGauge {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    // The entry sample is taken on the executor thread, which `start_write_executor` does not wait
    // for. Without this the first call could take that sample for its own and read a gauge older
    // than the event it was called to observe.
    while engine.write_executor_stats().wal.samples == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the executor never took its entry sample"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let before = engine.write_executor_stats().wal.samples;
    loop {
        rotate(engine);
        let now = engine.write_executor_stats().wal;
        if now.samples > before {
            return now;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the gauge never took another sample"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ---- I8: what a growth may not touch ------------------------------------------------------------

/// `clusters/a`, declaring corpus-derived supplied content — the declaration that gives an artifact
/// a generating set at all, and therefore the only one under which I8 has anything to say.
fn described_declaration() -> LayerDeclaration {
    let mut d = declaration("clusters/a");
    d.content.supplied = vec![tessera_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
    }];
    d
}

/// The artifacts one credential is served over the whole map — [`artifacts_of`] is the
/// full-coverage case of this, and a full-coverage mask contains every generating set, an empty or
/// a widened one included.
fn artifacts_for(engine: &Engine, credential: &[u8]) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// **I8's headline arm.** A generating set is immutable once supplied: documents arriving later are
/// members of the artifact and are **not** part of what its description was generated from.
///
/// The case is put to a principal that can tell the two apart. The generating set is drawn inside
/// the subset term, so a principal holding that term contains it entire and reads the description;
/// the joiners are the next thirty source ids, twenty of which that principal cannot see. If a
/// growth added the joiners to the set, the description would stop being readable by the one
/// principal it was published for — while every full-coverage assertion beside it went on holding.
///
/// **Mutations this kills:** any line beside `ArtifactStore::grow`'s
/// `record.members.or_inplace(joining)` that keeps a description's provenance "in sync" with the
/// membership it describes — `content.generated_from.or_inplace(joining)`. It fails conservatively
/// at first, which is why nothing else here would notice.
#[test]
fn a_growth_does_not_enter_the_generating_set() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(described_declaration()).unwrap();

    let covered = |s: &u64| terms_of(*s).contains(&SUBSET_TERM);
    assert!(
        (300..330).any(|s| !covered(&s)),
        "the joiners must include documents the narrow principal cannot see, or this test proves \
         nothing"
    );

    engine
        .publish_artifacts(
            "clusters/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("c0".into()),
                fx.members(0..300),
                vec![IncomingContent::new(
                    vec!["shipping and logistics".into()],
                    fx.members_of((0..300).filter(covered)),
                )],
            )],
        )
        .expect("a described artifact publishes");
    wait_for_publication(&fx, &engine, 1);

    let before = artifacts_for(&engine, &subset_credential());
    assert_eq!(
        before.len(),
        1,
        "the narrow principal contains the generating set entire, so it is served the artifact"
    );
    assert_eq!(before[0].content, vec!["shipping and logistics"]);
    let narrow_count_before = before[0].masked_count;

    grow(&fx, &engine, 300..330);

    assert_eq!(
        count(&engine),
        330,
        "the membership grew — without which the assertion below is about an artifact nothing \
         happened to"
    );

    let after = artifacts_for(&engine, &subset_credential());
    assert_eq!(
        after.len(),
        1,
        "the generating set is what it was published as, so the principal that contained it \
         contains it still: a joiner it cannot see must not have entered the set"
    );
    assert_eq!(
        after[0].content,
        vec!["shipping and logistics"],
        "and the description is still readable, which is the whole of what containment decides"
    );
    assert!(
        after[0].masked_count > narrow_count_before,
        "the narrow principal's own count did move, so the growth reached this viewer's answer \
         and left only the generating set alone"
    );
}

/// **The assertion the mechanism exists for.** A point joins a cluster that was already durable in
/// an extent, and it is still in the cluster after a restart.
///
/// The membership grew *below* its level's published high-water, so no tail pack will reach it and
/// the growth record is the only copy of the join until a fold. What this pins is that the record
/// is there to replay: the alternative is an artifact that comes back the size it was, acked and
/// quiet.
#[test]
fn a_point_that_joined_is_still_in_the_membership_after_a_restart() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        assert_eq!(count(&engine), 300);

        grow(&fx, &engine, 300..310);
        assert_eq!(
            count(&engine),
            310,
            "the artifact behaves as though the points had been there all along — a membership \
             that grew is not a membership that grows later"
        );
    }

    let engine = fx.open();
    assert_eq!(
        engine.published_artifacts(),
        1,
        "the artifact came back from the extent plus the log"
    );
    assert_eq!(
        count(&engine),
        310,
        "and it came back holding the points that joined it"
    );
}

/// **The packing half, and the decisive form of it.** The fold rewrites every level whole, so a
/// growth below the high-water reaches the prefix there and nowhere else. Deleting the log outright
/// is harsher than a rotation and settles the question: whatever comes back came from the manifest.
///
/// Without the fold's rewrite this is the silent failure — the log is the only home the join has,
/// so a reclaim leaves the artifact serving its pre-growth count with nothing anywhere to notice.
#[test]
fn a_growth_reaches_the_prefix_the_fold_publishes_and_outlives_the_log() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        grow(&fx, &engine, 300..310);
        assert_eq!(count(&engine), 310);

        fold(&engine);
        assert_eq!(count(&engine), 310, "the fold serves what it stored");
    }

    remove_the_whole_log(&fx.wal);

    let engine = fx.open();
    assert_eq!(engine.published_artifacts(), 1);
    assert_eq!(
        count(&engine),
        310,
        "the joined points are in the extent the fold wrote, with no log left to replay"
    );
}

/// **The pin holds until the fold, and a tail publication does not release it.** Between the growth
/// and the fold the log is the join's only home, so a publication that packed the level's tail —
/// and marked it published — must not let rotation past the growth record.
///
/// Asserted through a restart rather than through a counter: a second publication runs the whole
/// packing cycle, including the mark, and the restart then reads whatever survived it.
#[test]
fn a_later_publication_does_not_release_the_log_from_a_growth_below_it() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        grow(&fx, &engine, 300..310);

        // A second artifact into the same level: its extent is packed above the high-water the
        // grown record sits below, and its publication marks the level published to the top.
        engine
            .publish_artifacts(
                "clusters/a".into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some("c1".into()),
                    fx.members(400..410),
                )],
            )
            .unwrap();
        wait_for_publication(&fx, &engine, 2);
    }

    let engine = fx.open();
    let artifacts = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(artifacts.len(), 2, "both artifacts came back");
    let grown = artifacts
        .iter()
        .find(|a| a.key.as_deref() == Some("c0"))
        .expect("the grown artifact is one of them");
    assert_eq!(
        grown.masked_count, 310,
        "the growth survived a publication that packed the tail above it and marked the level \
         published — the mark covers what the packer wrote, never a record below where it started"
    );
}

/// **The pin, at the one place it bites: rotation.** A rotation seals the active member and
/// reclaims every sealed member entirely below the bound it is given — so with the log holding the
/// only copy of a join, the bound must sit at the growth record and not past it.
///
/// This is the failure written out: the member holding the growth is deleted, the artifact is still
/// in an extent from before the join, and the restart serves it at the size it was. Nothing is
/// logged, nothing is refused, and the artifact is indistinguishable from one whose criterion it
/// never cleared.
#[test]
fn a_rotation_may_not_reclaim_the_member_holding_a_growth() {
    let fx = fixture();
    {
        let engine = fx.open();
        publish(&fx, &engine, 0..300);
        grow(&fx, &engine, 300..310);
        // The growth is in whichever member was active when it was appended, which is the newest.
        let holding = wal_members(&fx.wal)
            .pop()
            .expect("the log has at least one member");

        // The buffer is empty — a publication writes no rows — so the tick has nothing to flush
        // and rotates instead. Twice, because the first seals the member the growth is in and the
        // second is the one that would find it sealed and reclaim it.
        rotate(&engine);
        rotate(&engine);
        assert!(
            wal_members(&fx.wal).contains(&holding),
            "{holding} holds the growth and is still there: rotation may not reclaim past it \
             while the log is the join's only home. Members now: {:?}",
            wal_members(&fx.wal)
        );
    }

    let engine = fx.open();
    assert_eq!(
        count(&engine),
        310,
        "and the join is what the restart reads back"
    );
}

/// **The pin the case above proves, as the number an operator reads.**
///
/// A log that cannot rotate and a log that is merely busy are the same size, and until the gauge
/// names the pin they are the same reading. `growth` is the name that matters: it is released by
/// the fold's whole rewrite and by nothing else, so a node whose fold is refused holds this pin for
/// as long as the refusal lasts (`compaction.md` §9).
///
/// The gauge is sampled at a tick and at most once per tick period, so every read here goes
/// through `resampled_gauge`, which ticks until the walk has run again.
#[test]
fn the_wal_gauge_names_the_growth_pin_and_the_fold_releases_it() {
    let fx = fixture();
    let engine = fx.open_with_short_tick();
    publish(&fx, &engine, 0..300);

    let before = resampled_gauge(&engine);
    assert!(
        before.members >= 1,
        "the log always has an active member; the gauge read {}",
        before.members
    );
    assert!(
        before.bytes > 0,
        "a member file carries a header at least; the gauge read {} bytes",
        before.bytes
    );

    grow(&fx, &engine, 300..310);

    let held = resampled_gauge(&engine);
    let (holder, at) = held.pin.expect(
        "the growth landed below its level's high-water, so it pins the log until a whole rewrite \
         covers it",
    );
    assert_eq!(
        holder, "growth",
        "the name is what says a tick will not release this one"
    );
    assert!(
        at <= held.position,
        "the pin sits at a record already written: pinned at {at}, log at {}",
        held.position
    );
    assert_eq!(
        held.pin_span_bytes,
        held.position - at,
        "the span is how much of the log the pin is holding down"
    );

    fold(&engine);
    assert_eq!(
        resampled_gauge(&engine).pin,
        None,
        "the fold rewrote the level whole, which is the one event that releases a growth"
    );
}

/// **A suppressed artifact still exists, so growth against it is ordinary and leaves it
/// suppressed** (`artifacts-from-points.md` §5). The key resolves in the store, which is what makes
/// this the boring case: written against the served view, a suppressed artifact would read as
/// absent and the ingest of a point would defeat the suppression.
#[test]
fn growing_a_suppressed_artifact_leaves_it_suppressed() {
    let fx = fixture();
    let engine = fx.open();
    let id = publish(&fx, &engine, 0..300);
    let entity = artifact_entity(&engine, id);

    engine.accept_change(entity, ChangeOp::Suppress).unwrap();
    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
        "the suppression is in force at the ack"
    );

    grow(&fx, &engine, 300..310);
    assert!(
        artifacts_of(&engine, &full_coverage_credential()).is_empty(),
        "and it is still in force: growth is not a route back into service"
    );

    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert_eq!(
        count(&engine),
        310,
        "the points joined while it was suppressed, exactly as they would have otherwise"
    );
}

/// A key the level does not hold is refused, naming the key. **On this route whatever the layer's
/// value set says**: a growth names an artifact to add members to, where a membership column names
/// the artifact a *point* belongs to and may therefore create it. There is no point here whose
/// column declared the key, so an unknown one is a typo with nothing behind it, and the honest
/// answer is that it names nothing — never an ack for a join into silence.
#[test]
fn an_unknown_key_is_refused_rather_than_minted() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);

    let refused = engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![IncomingGrowth::from_entities(
                "c-nope".into(),
                fx.members(300..310),
            )],
        )
        .expect_err("a key naming no artifact is refused")
        .to_string();
    assert!(
        refused.contains("c-nope"),
        "the refusal names the key that resolved to nothing: {refused}"
    );
    assert_eq!(count(&engine), 300, "and nothing joined anything");
}

/// **The whole batch or none of it.** One unresolvable key refuses the command rather than growing
/// the artifacts beside it, so a caller is never left unable to say which of their joins happened.
#[test]
fn one_unknown_key_refuses_the_whole_batch() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);

    engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![
                IncomingGrowth::from_entities("c0".into(), fx.members(300..310)),
                IncomingGrowth::from_entities("c-nope".into(), fx.members(310..320)),
            ],
        )
        .expect_err("the batch is refused");
    assert_eq!(
        count(&engine),
        300,
        "the resolvable join in the same batch did not happen either"
    );
}

/// A deleted entity may not join, on the same rule a publication declaring one is refused
/// (`annotation-write-cycle.md` §3.1): it can never contribute to a count again, so a join naming
/// one is refused loudly rather than applied into silence.
#[test]
fn a_deleted_point_may_not_join() {
    let fx = fixture();
    let engine = fx.open();
    publish(&fx, &engine, 0..300);

    let deleted = fx.members(300..301)[0];
    engine.accept_change(deleted, ChangeOp::Delete).unwrap();

    let refused = engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![IncomingGrowth::from_entities(
                "c0".into(),
                fx.members(300..310),
            )],
        )
        .expect_err("a deleted member refuses the join")
        .to_string();
    assert!(
        refused.contains("deleted"),
        "the refusal says what is wrong with it: {refused}"
    );
    assert_eq!(count(&engine), 300);
}

/// A member with no row is refused here exactly as it is at publication: it would count towards the
/// declared size the proportional criterion divides by while being visible to nobody.
#[test]
fn a_join_naming_something_other_than_a_point_is_refused() {
    let fx = fixture();
    let engine = fx.open();
    let id = publish(&fx, &engine, 0..300);
    let artifact = artifact_entity(&engine, id);

    engine
        .grow_memberships(
            "clusters/a".into(),
            0,
            vec![IncomingGrowth::from_entities("c0".into(), vec![artifact])],
        )
        .expect_err("an artifact is not a document");
    assert_eq!(count(&engine), 300);
}

// ---------------------------------------------------------------------------------------------
// Minting: the artifact an ingest batch's key created, after a restart
// ---------------------------------------------------------------------------------------------

/// A layer whose value set is **open**, so a key it does not hold creates the artifact it names
/// (`artifacts-from-points.md` §3).
fn open_declaration(name: &str) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        value_set: tessera_types::layer::ValueSet::Open,
        ..declaration(name)
    }
}

/// One ingest batch of one point, carrying the artifacts that point belongs to — what
/// `/control/ingest`'s membership column decodes to, taken here at the engine boundary so the
/// restart is a real reopen of a real log.
fn ingest_naming(engine: &Engine, batch: &str, layer: &str, key: &str) -> u64 {
    let descriptors = vec![b"0".to_vec()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(batch.as_bytes()) {
        *slot = *byte;
    }
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(batch.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    let (_, minted) = engine
        .accept_ingest_joining(
            vec![row],
            batch.to_string(),
            hash,
            tessera_lifecycle::BatchArtifacts {
                memberships: vec![tessera_lifecycle::BatchMembership {
                    layer: layer.to_string(),
                    level: 0,
                    key: key.to_string(),
                    rows: vec![0],
                }],
                edges: Vec::new(),
            },
        )
        .expect("a point naming an artifact of an open layer is an ordinary write");
    minted
}

/// **A minted artifact comes back from a restart, with the point that created it.**
///
/// Minting is a publication and rides the commit window's own fsync, so the record is in the log
/// before the caller is acked — but the record is appended by the *window*, which is a path no
/// publication took before, and the failure it would have is the one this file is written around:
/// an artifact that is silently absent, or back to a size it never had.
#[test]
fn an_artifact_a_batch_minted_survives_a_restart() {
    let fx = fixture();
    {
        let engine = fx.open();
        engine
            .register_layer(open_declaration("clusters/a"))
            .unwrap();
        assert_eq!(
            ingest_naming(&engine, "b1", "clusters/a", "made-by-a-point"),
            1,
            "the key named no artifact, so it created one"
        );
        // A second batch under the same key creates nothing and joins what the first made.
        assert_eq!(
            ingest_naming(&engine, "b2", "clusters/a", "made-by-a-point"),
            0
        );
        assert_eq!(engine.published_artifacts(), 1, "one key, one artifact");
        // Deliberately **not** folded: the record is in the log and nowhere else, which is the
        // state the restart below has to recover from.
    }

    let engine = fx.open();
    assert_eq!(
        engine.published_artifacts(),
        1,
        "the artifact its own points created is still there after a restart"
    );
    // A membership is projected through base rows, so the count is only readable once the ingested
    // points have them — which is what makes this the assertion rather than the one above.
    flush(&engine);
    fold(&engine);
    let artifacts = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].key.as_deref(), Some("made-by-a-point"));
    assert_eq!(
        artifacts[0].masked_count, 2,
        "and holds the points that created it — an artifact back at a size it never had is the \
         silent failure this file exists against"
    );
}

/// **A fold carries a minted artifact like any other**, which is the second half of the restart
/// case: the fold rewrites every level whole, so an artifact that entered the store through the
/// commit window rather than through a publication command must be written into the new prefix.
/// The log is deleted outright afterwards, so whatever comes back came from the prefix.
#[test]
fn a_minted_artifact_survives_the_fold_that_rewrites_its_level() {
    let fx = fixture();
    {
        let engine = fx.open();
        engine
            .register_layer(open_declaration("clusters/a"))
            .unwrap();
        assert_eq!(ingest_naming(&engine, "b1", "clusters/a", "c9"), 1);
        flush(&engine);
        fold(&engine);
    }
    remove_the_whole_log(&fx.wal);

    let engine = fx.open();
    let artifacts = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(
        artifacts.len(),
        1,
        "the fold carried it into the new prefix"
    );
    assert_eq!(artifacts[0].key.as_deref(), Some("c9"));
    assert_eq!(artifacts[0].masked_count, 1);
}

/// **A key the layer's value set does not admit is refused, and the batch has no effect** — the
/// closed default, at the engine boundary where the row would have been allocated.
#[test]
fn a_closed_layers_unknown_key_refuses_the_batch() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration("clusters/a")).unwrap();

    let before = engine.allocator_high_water();
    let descriptors = vec![b"0".to_vec()];
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(b"p1".to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    let refused = engine
        .accept_ingest_joining(
            vec![row],
            "b1".to_string(),
            [1u8; 32],
            tessera_lifecycle::BatchArtifacts {
                memberships: vec![tessera_lifecycle::BatchMembership {
                    layer: "clusters/a".to_string(),
                    level: 0,
                    key: "never-declared".to_string(),
                    rows: vec![0],
                }],
                edges: Vec::new(),
            },
        )
        .expect_err("a closed layer's roster is its artifacts")
        .to_string();
    assert!(refused.contains("never-declared"), "{refused}");
    assert!(
        refused.contains("value_set"),
        "and says what would admit it: {refused}"
    );
    assert_eq!(
        engine.allocator_high_water(),
        before,
        "the batch had no effect at all — not even an entity id"
    );
}

// ---------------------------------------------------------------------------------------------
// Restating a membership: the join that names an artifact its entity is already in
// ---------------------------------------------------------------------------------------------

/// One ingest batch of one point in `view`, joining whatever entity the external id already names
/// and carrying the artifact the point belongs to. The join is the server's own: `/control/ingest`
/// resolves an established external id to its entity and the executor settles it, so a second view
/// of one item arrives here exactly like this.
fn ingest_into_view(engine: &Engine, batch: &str, view: &str, external_id: &str, key: &str) {
    let descriptors = vec![b"0".to_vec()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(batch.as_bytes()) {
        *slot = *byte;
    }
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: view.to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest_joining(
            vec![row],
            batch.to_string(),
            hash,
            tessera_lifecycle::BatchArtifacts {
                memberships: vec![tessera_lifecycle::BatchMembership {
                    layer: "clusters/a".to_string(),
                    level: 0,
                    key: key.to_string(),
                    rows: vec![0],
                }],
                edges: Vec::new(),
            },
        )
        .expect("a point naming an artifact of an open layer is an ordinary write");
}

/// **A join that restates a membership the artifact already holds appends nothing.**
///
/// A second view of one item is a join: it carries the entity the external id already names, and
/// its membership column names the artifact that entity is already in. The growth that would be
/// written for it adds no member, so it changes nothing — and a record that changes nothing still
/// pins the log at itself until a fold rewrites the level, which is the one thing a pin costs.
/// `POST /control/values` subtracts what the artifact already holds for this reason; the question
/// here is the same one at the ingest door.
///
/// The fold before the second batch is what makes the pin the assertion: it releases every growth
/// pin the first batch left, so anything the gauge names afterwards was written by the join.
#[test]
fn a_joining_row_restating_its_membership_appends_no_growth() {
    let fx = fixture();
    let engine = fx.open_with_short_tick();
    engine
        .create_plain_view(tessera_engine::PlainViewDeclaration {
            name: "s1".to_string(),
            title: None,
            projection: "none".to_string(),
            frame: tessera_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
        })
        .expect("the second view is created");
    engine
        .register_layer(LayerDeclaration {
            views: vec!["s0".into(), "s1".into()],
            ..open_declaration("clusters/a")
        })
        .expect("the layer is declared over both views");

    ingest_into_view(&engine, "b1", "s0", "p1", "c9");
    flush(&engine);
    fold(&engine);
    assert_eq!(
        resampled_gauge(&engine).pin,
        None,
        "the fold rewrote the level whole, so nothing pins the log going in"
    );

    ingest_into_view(&engine, "b2", "s1", "p1", "c9");

    assert_eq!(
        engine.published_artifacts(),
        1,
        "one key, one artifact: the join created nothing"
    );
    assert_eq!(
        resampled_gauge(&engine).pin,
        None,
        "the entity is already a member, so the join has nothing to write and nothing to pin the \
         log with"
    );
}
