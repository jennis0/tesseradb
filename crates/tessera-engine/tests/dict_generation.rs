//! The dictionary is generation-scoped (§3.2).
//!
//! A flush promotes each novel descriptor to a durable dictionary ordinal and publishes the
//! assignment as a `dict_extents` entry. That is only expressible if the dictionary can be
//! republished with a generation, which it could not while it was bound at `Engine::open`.

mod common;

use std::sync::Arc;

use common::*;
use tessera_authz::{Dict, DictWriter};
use tessera_engine::GeometryPublication;
use tessera_types::TermId;

/// Ordinals are stable across an extension: a term that resolved to 3 before still does, or
/// every session authorised before the flush is now evaluating against a different term.
///
/// This is §3.4's premise 3 in its structural form — the equality of a patch and a rebuild
/// rests on a session's `satisfied` naming the same terms after a flush as before it.
#[test]
fn extending_a_dict_preserves_every_existing_ordinal() {
    let dir = tempfile::TempDir::new().unwrap();
    let dict = dict_in(&dir.path().join("base"), &[b"a", b"b", b"c"]);
    let extent = extent_in(&dir.path().join("ext"), &[b"d", b"e"]);

    let extended = dict.load_extending(&extent).unwrap();

    for (i, d) in [b"a".as_slice(), b"b", b"c"].iter().enumerate() {
        assert_eq!(extended.lookup(d), Some(TermId::new(i as u32)), "{d:?}");
    }
    assert_eq!(extended.lookup(b"d"), Some(TermId::new(3)));
    assert_eq!(extended.lookup(b"e"), Some(TermId::new(4)));
    assert_eq!(extended.len(), 5);
}

/// The write path resolves against the generation's dictionary too, so a descriptor a flush has
/// promoted stops being minted a fresh extension id on every later arrival.
///
/// Extension ids count down from `u32::MAX` and are unsatisfiable by construction; a promoted
/// ordinal is an ordinary dictionary term. The difference between them is the whole of what
/// promotion buys, and this is where the write path is observed to see it.
#[test]
fn the_write_path_resolves_against_the_generations_dict() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    let novel = vec![b"novel".to_vec()];
    let before = engine.resolve_terms(&novel);
    assert!(
        before[0].raw() > 200_000_000,
        "an unpromoted descriptor gets an unsatisfiable extension id, not an ordinal"
    );

    let live = engine.generation();
    let promoted_ordinal = live.dict.len();
    let extended = Arc::new(
        live.dict
            .load_extending(&extent_in(&tmp.path().join("promoted"), &[b"novel"]))
            .unwrap(),
    );
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            live.prefix.clone(),
            live.segments_version + 1,
            live.watermark,
            Arc::clone(&live.bundle),
            extended,
            Vec::new(),
        ))
        .unwrap();

    assert_eq!(
        engine.resolve_terms(&novel),
        vec![TermId::new(promoted_ordinal)],
        "after the promotion the write path resolves the durable ordinal"
    );
}

/// Authorise reads the generation's dictionary, not a process-lifetime one — so a descriptor
/// promoted by a publication is satisfiable by the sessions authorised after it.
///
/// A promoted ordinal sits at or above the base postings' term count, so this only works because
/// a term no file carries reads as an empty posting rather than an error (§5.2) — the property
/// that makes a sparse delta tier possible at all.
#[test]
fn authorise_resolves_against_the_generations_dict() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    // "0" is in the fixture's dictionary; "novel" is not, so it drops out of `satisfied`.
    let credential = br#"{"terms": ["0", "novel"]}"#.to_vec();
    let before = engine.authorise(&credential).unwrap();
    assert_eq!(
        resolved(&before),
        1,
        "an unknown descriptor is simply unsatisfied, never an error"
    );

    let live = engine.generation();
    let extended = Arc::new(
        live.dict
            .load_extending(&extent_in(&tmp.path().join("promoted"), &[b"novel"]))
            .unwrap(),
    );
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            live.prefix.clone(),
            live.segments_version + 1,
            live.watermark,
            Arc::clone(&live.bundle),
            extended,
            Vec::new(),
        ))
        .unwrap();

    let after = engine.authorise(&credential).unwrap();
    assert_eq!(
        resolved(&after),
        2,
        "a promoted descriptor is satisfiable by a session authorised after the publication"
    );
    assert_eq!(
        resolved(&before),
        1,
        "and the session authorised before it is untouched — `satisfied` is never re-resolved, \
         which is what §3.4's patch-equals-a-rebuild rests on"
    );
}

fn dict_in(dir: &std::path::Path, descriptors: &[&[u8]]) -> Dict {
    Dict::load(&extent_in(dir, descriptors)).unwrap()
}

fn extent_in(dir: &std::path::Path, descriptors: &[&[u8]]) -> Vec<std::path::PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    let mut writer = DictWriter::new(dir);
    for d in descriptors {
        writer.intern(d);
    }
    writer.finish().unwrap()
}

/// A viewport, with `ProjectionBuilding` retried on a bounded deadline. The shed is decision
/// 0058's park-then-shed exhausting its wait budget — documented on the variant as retryable and
/// mapped to a 429 with `Retry-After` at the server boundary — so a conforming caller retries,
/// and this test is not the place to treat the shed as a verdict.
fn viewport_settling(
    engine: &tessera_engine::Engine,
    session: &tessera_engine::Session,
) -> tessera_engine::ViewportOut {
    use tessera_engine::ViewportRequest;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let request = ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize);
        match engine.viewport(session, request) {
            Ok(response) => return response,
            Err(tessera_engine::EngineError::ProjectionBuilding)
                if std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("the viewport neither answered nor kept shedding: {e:?}"),
        }
    }
}

/// **A session authorised after a promoting flush must be served the post-flush visible set**
/// ([#112](https://github.com/jennis0/tessera-index/issues/112)).
///
/// The defect this pins is not staleness in the ordinary sense — nothing later repaired it. A
/// credential naming a descriptor the dictionary did not yet hold, re-presented *after* the flush
/// that promoted it, went on being served the pre-flush set indefinitely, while a credential whose
/// bytes differed but whose satisfied set was identical was served the correct one at the same
/// instant. That asymmetry is the signature: the only thing keyed on the credential's *bytes* is
/// `FragmentCache`'s memo from `auth_data_hash` to the canonical fragment key.
///
/// **The poisoning step is the one a reader would leave out.** The memo is written by whoever
/// misses it first, and `Engine::fragment_for` misses it on behalf of the *already-authorised*
/// session — carrying that session's `satisfied`, frozen at authorise, together with the dictionary
/// length of the generation the request is running against. Those two do not belong to each other
/// once a promotion has landed, and `get_or_build`'s caller obligation says so: the hash and the
/// length "must never arrive paired with two different term sets". The stale pair is memoised, and
/// the next authorise of the same bytes — which resolves the descriptor correctly — takes a hit on
/// it and is handed the fragment for the grant set it has just stopped having.
///
/// So the viewport below is not incidental to the reproduction; without it the memo entry is never
/// written and the bug does not appear.


#[test]
fn a_credential_re_presented_after_a_promoting_flush_sees_the_promoted_descriptor() {
    use tessera_lifecycle::UnallocatedRow;

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    // `novel` is not in the built dictionary, so it drops out of `satisfied` here.
    let credential = br#"{"terms": ["0", "novel"]}"#.to_vec();
    let stale = engine.authorise(&credential).unwrap();
    assert_eq!(resolved(&stale), 1, "the fixture must not know `novel`");
    let before = stale.fragment_at_authorise_for_test().view().cardinality();

    // Ingest under the novel descriptor and flush, which promotes it and mints its term. The rows
    // are the entities the promoted term will carry — without them the promotion adds an empty
    // posting and every assertion below is vacuous.
    for i in 0..8u32 {
        let external = format!("novel-{i}");
        let row = UnallocatedRow {
            external_id: Some(external.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"novel".to_vec()],
            x: 10.0 + i as f64,
            y: 10.0 + i as f64,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"novel".to_vec()]),
            scoped: Vec::new(),
        };
        engine
            .accept_ingest(vec![row], external, [0u8; 32])
            .expect("an ingest under a novel descriptor is accepted");
    }
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == 0 {
        assert!(std::time::Instant::now() < deadline, "the flush never landed");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        engine.generation().dict.lookup(b"novel").is_some(),
        "the flush must have promoted `novel`, or this tests nothing"
    );

    // **The poisoning step.** The pre-flush session asks for a viewport, which brings its frozen
    // `satisfied` forward against the *new* generation.
    viewport_settling(&engine, &stale);

    // Same bytes, re-presented. This resolves `novel` and must be served its posting.
    let fresh = engine.authorise(&credential).unwrap();
    assert_eq!(resolved(&fresh), 2, "the promoted descriptor resolves");

    // Different bytes, identical satisfied set — the control the issue names. It cannot take the
    // memo's fast path, so it is the answer the line above must agree with.
    let other = br#"{"terms": ["0", "novel", "also-unknown"]}"#.to_vec();
    let control = engine.authorise(&other).unwrap();
    assert_eq!(control.satisfied_for_test(), fresh.satisfied_for_test(), "the control's premise");

    assert_eq!(
        fresh.fragment_at_authorise_for_test().view().cardinality(),
        control.fragment_at_authorise_for_test().view().cardinality(),
        "two credentials with the same satisfied term set were served different visible sets; the \
         one whose bytes were seen before the promotion took a stale memo hit"
    );
    assert!(
        fresh.fragment_at_authorise_for_test().view().cardinality() > before,
        "the re-presented credential must gain the promoted descriptor's entities"
    );
}

/// **The same obligation at the background refresh** (#112), which is the call site that made the
/// defect look as though it had no trigger.
///
/// `refresh_resident` rebuilds every resident session's fragment after a publication, unprompted
/// and on a timer, from a `satisfied` frozen at that session's authorise. So in a running
/// deployment nobody has to make the request that writes the bad memo entry — the refresh makes it,
/// for every session holding a projection, on the first flush after any of them named a descriptor
/// the dictionary did not yet carry.
///
/// Distinct from the test above rather than a duplicate of it: that one drives
/// `Engine::fragment_for` from a request, this one drives the refresh, and each was passing the
/// live generation's dictionary length where the frozen term set's own generation was owed. The
/// second was found by the compiler after the first was fixed, which is the argument for pinning
/// both.
#[test]
fn a_background_refresh_does_not_poison_a_later_authorise_of_the_same_credential() {
    use std::time::{Duration, Instant};
    use tessera_engine::{Engine, EngineConfig};
    use tessera_lifecycle::UnallocatedRow;

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = {
        let mut engine = Engine::open(
            &root,
            &tmp.path().join("cache"),
            &tmp.path().join("wal.log"),
            tessera_plugin::Passthrough::new(),
            EngineConfig {
                flush_max_age_secs: 1,
                // The shipped row trigger, four commit windows (`DEFAULT_FLUSH_MAX_ITEMS`):
                // what bounds the window close's O(buffered) copy. Nothing here reaches it.
                flush_max_items: 40_000,
                compaction: tessera_engine::CompactionSchedule::off(),
                ..config_uncapped()
            },
        )
        .expect("engine opens");
        engine.start_write_executor(64).expect("the executor starts");
        engine
    };

    let credential = br#"{"terms": ["0", "novel"]}"#.to_vec();
    let stale = engine.authorise(&credential).unwrap();
    assert_eq!(resolved(&stale), 1, "the fixture must not know `novel`");

    // A resident projection is what makes this session visible to the refresh at all.
    viewport_settling(&engine, &stale);

    for i in 0..8u32 {
        let external = format!("refresh-novel-{i}");
        let row = UnallocatedRow {
            external_id: Some(external.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"novel".to_vec()],
            x: 10.0 + i as f64,
            y: 10.0 + i as f64,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"novel".to_vec()]),
            scoped: Vec::new(),
        };
        engine
            .accept_ingest(vec![row], external, [0u8; 32])
            .expect("an ingest under a novel descriptor is accepted");
    }
    engine.request_flush();

    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.refreshes() == 0 {
        assert!(
            Instant::now() < deadline,
            "the background refresh never ran, so this asserts nothing"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(engine.generation().dict.lookup(b"novel").is_some());

    // Nobody has issued a request since the flush. The refresh alone must not have left an entry
    // that answers for this credential.
    let fresh = engine.authorise(&credential).unwrap();
    let control = engine
        .authorise(br#"{"terms": ["0", "novel", "also-unknown"]}"#)
        .unwrap();
    assert_eq!(fresh.satisfied_for_test(), control.satisfied_for_test(), "the control's premise");
    assert_eq!(
        fresh.fragment_at_authorise_for_test().view().cardinality(),
        control.fragment_at_authorise_for_test().view().cardinality(),
        "the background refresh poisoned the canonical-key memo for this credential"
    );
}

/// The credential's own resolved descriptors: `satisfied` minus the reserved `public` term.
///
/// **Every session holds `public` by construction** (`per-point-attributes.md` §3.8), added inside
/// the engine rather than by the credential — so counting `satisfied` directly would count a term
/// this file's cases are not about, in every one of them.
fn resolved(session: &tessera_engine::Session) -> usize {
    assert!(
        session.satisfied_for_test().contains(&tessera_authz::PUBLIC_TERM),
        "every session holds the reserved `public` term"
    );
    session.satisfied_for_test().len() - 1
}
