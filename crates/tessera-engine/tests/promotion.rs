//! A novel descriptor becomes a durable ordinal at flush (§3.2).
//!
//! `buffer.rs` mints ids for descriptors the dictionary has never seen counting down from
//! `u32::MAX`, so they are unsatisfiable by construction — an item can buffer under one but can
//! never be seen through it. The flush is where that ends: it turns each extension id back into
//! its descriptor, assigns a durable ordinal, writes the tier's postings under it and publishes
//! the assignment as a `dict_extents` entry.
//!
//! **Where the descriptor bytes come from.** `BufferedItem` holds resolved `TermId`s, so the
//! buffer alone cannot answer this. `DescriptorResolver`'s extension map can, and has all along:
//! one entry per distinct descriptor, kept for the process's lifetime so the assignment stays
//! continuous across replay and live accepts. `LiveState::descriptors_of` is the inverse of it.
//!
//! **Every assertion that matters is made on a fresh open of the bundle**, because the failure
//! this mechanism most needs to exclude is a running process and the same bundle after a restart
//! disagreeing about what an ordinal means. That was a real fail-open — `Dict::load` counted a
//! repeated descriptor that `Dict::load_extending` skipped, shifting every ordinal after it — and
//! `tessera-authz`'s own tests pin the loader law. These pin what a flush writes.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::UnallocatedRow;
use tessera_plugin::{AuthTerms, DeclaredBounds, Descriptor, Passthrough, Plugin, PluginError};
use tessera_types::EntityId;

/// [`Passthrough`] with a lower declared term ceiling, for the one bound promotion enforces.
///
/// A wrapper rather than a field on `Passthrough`: the mapping is identical and so are both
/// plugin hashes — which matters, because a different hash would make `Engine::open` refuse the
/// fixture rather than exercise the ceiling.
#[derive(Debug, Clone, Copy)]
struct CappedTerms(u64);

impl Plugin for CappedTerms {
    fn terms_of_label(&self, access: &[u8]) -> Result<Vec<Descriptor>, PluginError> {
        Passthrough::new().terms_of_label(access)
    }
    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<AuthTerms, PluginError> {
        Passthrough::new().terms_of_auth(auth_data)
    }
    fn declared_bounds(&self) -> DeclaredBounds {
        DeclaredBounds {
            max_distinct_terms: self.0,
            ..Passthrough::new().declared_bounds()
        }
    }
    fn data_plugin_hash(&self) -> String {
        Passthrough::new().data_plugin_hash()
    }
    fn auth_plugin_hash(&self) -> String {
        Passthrough::new().auth_plugin_hash()
    }
}

/// Descriptors no fixture dictionary carries.
const NOVEL: &[u8] = b"dept:secret";
const OTHER: &[u8] = b"dept:legal";

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An engine on `root`, keeping its cache and WAL under `tmp`.
///
/// A restart case passes a **fresh** `tmp`, which is what makes its assertions mean something: with
/// an empty WAL nothing replays, so a dictionary the reopened engine holds can only have come from
/// the bundle's own `dict_extents`.
fn engine_at(tmp: &Path, root: &Path, tick_secs: u64) -> Engine {
    std::fs::create_dir_all(tmp).expect("the runtime directory exists");
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            max_merged_segment_bytes: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn fixture(tmp: &Path) -> PathBuf {
    let root = tmp.join("bundle");
    build_fixture(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    root
}

/// Ingest one item at (5, 5) carrying exactly `descriptors`.
fn ingest_with(engine: &Engine, external_id: &str, descriptors: &[&[u8]]) -> EntityId {
    let descriptors: Vec<Vec<u8>> = descriptors.iter().map(|d| d.to_vec()).collect();
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        slice: "s0".to_string(),
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

fn flushes_reach(engine: &Engine, n: u64) {
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= n
    });
}

/// The fixture build's own dictionary extent, which is always `dict_extents[0]`. Ordinals are
/// positions in the concatenation of the extents in listed order, so a promotion appends after
/// this and never disturbs it — which is exactly what these tests are checking.
fn base_extent() -> Vec<Vec<u8>> {
    vec![b"0".to_vec(), b"1".to_vec()]
}

/// The descriptors a partition's dict extents carry, in listed order — read off disk, so this
/// asserts the artefact rather than a lookup's opinion of it.
fn extent_records(root: &Path, prefix: &str) -> Vec<Vec<Vec<u8>>> {
    let bundle = tessera_store::open_bundle(root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    let prefix_dir = root.join(prefix);
    partition
        .manifest
        .dict_extents
        .iter()
        .map(|extent| {
            let data = std::fs::read(prefix_dir.join(&extent.path)).expect("extent reads");
            let mut out = Vec::new();
            let mut offset = 0;
            while offset < data.len() {
                let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;
                out.push(data[offset..offset + len].to_vec());
                offset += len;
            }
            out
        })
        .collect()
}

/// **Obligation 1 and 2, the whole mechanism**: an item ingested under a descriptor the dictionary
/// has never seen becomes visible through it — to a session authorised *after* the flush, and to a
/// process that restarts onto the bundle.
///
/// The before/after pair is §3.2's first fail-closed consequence, and it must keep holding:
/// `satisfied` is fixed at authorise, so a session that predates the promotion evaluates the terms
/// it was granted and no others.
#[test]
fn a_novel_descriptor_becomes_a_durable_ordinal_and_the_item_becomes_visible() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    let credential = br#"{"terms": ["dept:secret"]}"#.to_vec();
    let before = engine.authorise(&credential).expect("authorises");
    assert!(
        before.satisfied.is_empty(),
        "the descriptor does not exist yet, so this session satisfies nothing"
    );

    let id = ingest_with(&engine, "ext-1", &[NOVEL]);
    flushes_reach(&engine, 1);

    // On disk: the assignment is published as a `dict_extents` entry naming exactly the one
    // descriptor this flush interned.
    assert_eq!(
        extent_records(&root, &engine.generation().prefix),
        vec![base_extent(), vec![NOVEL.to_vec()]],
        "the build's extent, then one the flush appended carrying exactly the promoted descriptor"
    );

    // **Restart equality** (obligation 2). The reopened dictionary must give the descriptor the
    // same ordinal the publishing process assigned, or the tier's postings name something else.
    let reopened = engine_at(&tmp.path().join("restart"), &root, 3600);
    assert_eq!(
        reopened.generation().dict.lookup(NOVEL),
        engine.generation().dict.lookup(NOVEL),
        "the reopened bundle resolves the promoted descriptor to the ordinal the flush assigned"
    );
    assert_eq!(
        reopened.generation().dict.len(),
        engine.generation().dict.len()
    );

    // And it means something: the item is visible through the promoted term.
    let after = reopened.authorise(&credential).expect("authorises");
    assert_eq!(after.satisfied.len(), 1, "the descriptor now resolves");
    let tessera_id = reopened.tessera_id_of(id).expect("identity is computable");
    assert!(
        reopened.item(&after, tessera_id, None).unwrap().is_some(),
        "the flushed item is visible through the descriptor it was ingested under"
    );

    // §3.2's first consequence, preserved: the older session never gains it.
    assert!(
        before.satisfied.is_empty(),
        "`satisfied` is fixed at authorise; a promotion never reaches back into a live session"
    );
}

/// **Obligation 4**: the extent never carries a descriptor the dictionary already holds.
///
/// This is the writer half of the no-duplicate rule, and the reason it is asserted on the file
/// rather than through a lookup: a duplicate is invisible to `lookup` in the process that wrote it
/// and only appears after a restart, as every ordinal past the repeat shifting by one. A session
/// granted one term is then served another's items.
///
/// **Obligation 6** rides the same fixture: a descriptor promoted by the first flush and carried by
/// an item in the second gets the first's ordinal and produces no second record.
#[test]
fn a_second_flush_reuses_the_first_flushs_ordinal_and_writes_no_duplicate_record() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    ingest_with(&engine, "ext-1", &[NOVEL]);
    flushes_reach(&engine, 1);
    let promoted = engine
        .generation()
        .dict
        .lookup(NOVEL)
        .expect("the first flush promoted it");

    // A second item under the *same* descriptor, plus one genuinely new.
    ingest_with(&engine, "ext-2", &[NOVEL, OTHER]);
    flushes_reach(&engine, 2);

    assert_eq!(
        engine.generation().dict.lookup(NOVEL),
        Some(promoted),
        "the ordinal the first flush assigned is unchanged by the second (obligation 5)"
    );
    assert_eq!(
        extent_records(&root, &engine.generation().prefix),
        vec![base_extent(), vec![NOVEL.to_vec()], vec![OTHER.to_vec()]],
        "the second flush's extent carries only what was genuinely novel — no duplicate record, \
         which is what keeps every ordinal after it stable across a restart"
    );

    // The artefact is what a restart reads, so prove the ordinals survive it.
    let reopened = engine_at(&tmp.path().join("restart"), &root, 3600);
    assert_eq!(reopened.generation().dict.lookup(NOVEL), Some(promoted));
    assert_eq!(
        reopened.generation().dict.lookup(OTHER),
        engine.generation().dict.lookup(OTHER)
    );
}

/// **Obligation 8**: with promotion built, §3.3's staleness hint is reachable by an ingest — the
/// path it was specified for. `tests/staleness_hint.rs` drives the condition directly; this is the
/// end-to-end half it could not have.
#[test]
fn an_ingest_and_a_tick_flip_the_staleness_hint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 1);

    let session = engine
        .authorise(br#"{"terms": ["0", "dept:secret"]}"#)
        .expect("authorises");
    assert_eq!(session.satisfied.len(), 1, "one resolved, one did not");
    assert!(!session.is_stale(&engine.generation()));

    ingest_with(&engine, "ext-1", &[NOVEL]);
    flushes_reach(&engine, 1);

    assert!(
        session.is_stale(&engine.generation()),
        "a promoting flush is what the hint was specified to advertise"
    );
}

/// **Obligation 7**: promotion past the plugin's declared `max_distinct_terms` fails the flush and
/// retains the buffer.
///
/// The bound is enforced rather than declared for one reason: `EXTENSION_ID_START >
/// max_distinct_terms` is what keeps a dictionary ordinal from ever aliasing a live extension id,
/// and promotion is the only path by which a caller grows the dictionary at all.
///
/// Driven through a plugin declaring a ceiling the fixture's dictionary already sits at, since the
/// real one is 200,000,000.
#[test]
fn promotion_past_the_declared_term_ceiling_refuses_the_flush() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        // The fixture's dictionary already holds two terms, so any promotion is over the line.
        CappedTerms(2),
        EngineConfig {
            flush_max_age_secs: 1,
            max_merged_segment_bytes: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts");

    let id = ingest_with(&engine, "ext-1", &[NOVEL]);
    wait_until("the flush to fail", || {
        engine.write_executor_stats().flush_failures >= 1
    });

    assert_eq!(
        engine.write_executor_stats().flushes,
        0,
        "nothing published: a refused promotion is a failed flush, not a partial one"
    );
    assert!(
        engine.generation().buffer.contains(id),
        "and the row is retained, so ingest backpressure is what sheds — not the item"
    );
    assert_eq!(
        extent_records(&root, &engine.generation().prefix),
        vec![base_extent()],
        "the build's extent and nothing else: a refused promotion commits no extent"
    );
}
