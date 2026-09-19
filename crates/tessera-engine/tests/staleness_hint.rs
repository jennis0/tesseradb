//! §3.3: a session learns its mask is stale, and chooses when to act.
//!
//! A promotion gives a descriptor a durable dictionary ordinal (§3.2), and every session
//! authorised before it keeps the `satisfied` set it resolved — so it under-sees, fail-closed and
//! with no way to find out. These three cases are what the hint is: it fires when a promotion has
//! left this session behind, it never fires for a session with nothing unresolved, and no request
//! fails on it.
//!
//! **The promotion is driven by hand here, and that is not a shortcut.** A flush is what will
//! promote, but it cannot yet — `flush::promote` carries the ⊘: `BufferedItem` holds resolved
//! `TermId`s and not the descriptor bytes, so an extension id cannot be turned into an ordinal at
//! flush time and a novel term is left out of the tier entirely. What a flush *does* with the
//! dictionary it produces is publish it, and `Engine::publish_geometry` is that same call — the
//! executor sets `Generation::dict` from `CompletedFlush::dict` through it. So the observable
//! these cases assert is exactly the one a promoting flush will produce; what is missing upstream
//! is the promotion itself, not the publication.
//!
//! **The dictionary hint, not the geometry stamp.** `geometry-pinning.md` §7's staleness stamp
//! answers "has geometry moved since you last looked"; this answers "has a descriptor you were
//! granted but which did not exist become real". Different mechanisms, different questions.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use tessera_authz::DictWriter;
use tessera_engine::GeometryPublication;
use tessera_engine::{Engine, Session, ViewportRequest};

/// A descriptor no fixture dictionary carries, so it resolves to `None` at authorise.
const NOVEL: &[u8] = b"dept:secret";

/// The fixture's `ALL_TERM` — which resolves — plus [`NOVEL`], which does not.
const PARTLY_UNRESOLVED: &[u8] = br#"{"terms": ["0", "dept:secret"]}"#;

/// The fixture's `ALL_TERM`, named twice. The passthrough plugin hands descriptors on verbatim and
/// deduplicates nothing, so this is two descriptors resolving to one ordinal.
const DOUBLED: &[u8] = br#"{"terms": ["0", "0"]}"#;

fn engine_on_fixture(tmp: &Path) -> (Engine, PathBuf) {
    let root = tmp.join("bundle");
    build_fixture(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    let engine = open_engine_publishing(&root, &tmp.join("cache"), &tmp.join("wal.log"));
    (engine, root)
}

/// Publish a generation whose dictionary carries `descriptor` — the promotion a flush will do
/// once it can (see this file's module doc). `dir` names a fresh directory for the extent.
fn promote(engine: &Engine, dir: &Path, descriptor: &[u8]) {
    std::fs::create_dir_all(dir).unwrap();
    let mut writer = DictWriter::new(dir);
    writer.intern(descriptor);
    let extent = writer.finish().unwrap();

    let live = engine.generation();
    // Ordinals are preserved across an extension, which is what lets a session authorised before
    // this keep evaluating the terms it was granted (§3.4's premise 3).
    let extended = Arc::new(live.dict.load_extending(&extent).unwrap());
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            live.prefix.clone(),
            live.segments_version + 1,
            live.watermark,
            Arc::clone(&live.bundle),
            extended,
            Vec::new(),
        ))
        .expect("the publication is accepted");
}

/// The condition is already computed and discarded: the descriptors that resolved to `None` at
/// authorise are precisely the session's exposure to promotion.
#[test]
fn a_session_holding_an_unresolved_descriptor_is_hinted_by_a_promotion() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (engine, _root) = engine_on_fixture(tmp.path());

    let session: Session = engine.authorise(PARTLY_UNRESOLVED).expect("authorises");
    assert_eq!(
        resolved(&session),
        1,
        "one descriptor resolved and one did not, or this test proves nothing"
    );
    assert!(
        !session.is_stale(&engine.generation()),
        "nothing has been promoted yet"
    );

    promote(&engine, &tmp.path().join("promoted"), NOVEL);

    assert!(
        session.is_stale(&engine.generation()),
        "a promotion moved the dictionary past where this session resolved its terms"
    );
    assert_eq!(
        resolved(&session),
        1,
        "and the hint is telling the truth: this session's mask still omits the promoted term, \
         which is the under-seeing it exists to advertise — `satisfied` is never re-resolved"
    );

    // The only remedy is a new session.
    let fresh = engine.authorise(PARTLY_UNRESOLVED).expect("authorises");
    assert_eq!(resolved(&fresh), 2, "the promoted descriptor resolves");
    assert!(
        !fresh.is_stale(&engine.generation()),
        "and re-authorising clears it"
    );
}

/// The common case, and the one that must not regress: a session with nothing unresolved is
/// **never** hinted, however much the dictionary grows.
#[test]
fn a_session_with_no_unresolved_descriptors_is_never_hinted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (engine, _root) = engine_on_fixture(tmp.path());

    let session = engine
        .authorise(&full_coverage_credential())
        .expect("authorises");
    let dict_len_before = engine.generation().dict.len();

    promote(&engine, &tmp.path().join("promoted"), b"dept:unrelated");

    assert!(
        engine.generation().dict.len() > dict_len_before,
        "the dictionary must have grown, or this test proves nothing"
    );
    assert!(
        !session.is_stale(&engine.generation()),
        "this session left no descriptor unresolved, so no promotion can be one of its own"
    );
}

/// Advisory: no request fails on it, and nothing forces a rebuild.
#[test]
fn a_hinted_session_continues_to_serve() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (engine, _root) = engine_on_fixture(tmp.path());

    let session = engine.authorise(PARTLY_UNRESOLVED).expect("authorises");
    promote(&engine, &tmp.path().join("promoted"), NOVEL);
    assert!(session.is_stale(&engine.generation()));

    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 6, [0.0, 0.0, 1000.0, 1000.0], 200),
        )
        .expect("a hinted session serves exactly as it did before");
    assert!(
        out.tiles.iter().map(|t| t.visible).sum::<u64>() > 0,
        "and it still sees everything its unchanged mask covers"
    );
}

/// Two descriptors that resolve to one term leave nothing unresolved.
///
/// `satisfied` is a set, so the credential's descriptor count is not the number of terms it
/// resolved to: a session that named the same descriptor twice must be hinted by no promotion, and
/// must see exactly what the same credential naming it once sees. Counting the difference instead
/// would hint this session for ever, and a hint that fires for a session with nothing missing is
/// the one shape that makes the advisory worthless.
#[test]
fn a_credential_naming_one_descriptor_twice_is_never_hinted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (engine, _root) = engine_on_fixture(tmp.path());

    let doubled = engine.authorise(DOUBLED).expect("authorises");
    let once = engine
        .authorise(&full_coverage_credential())
        .expect("authorises");
    assert_eq!(
        resolved(&doubled),
        1,
        "two descriptors, one term — or this test is about something else"
    );
    assert_eq!(
        visible(&engine, &doubled),
        visible(&engine, &once),
        "the duplicate changes nothing about what is served"
    );

    let dict_len_before = engine.generation().dict.len();
    promote(&engine, &tmp.path().join("promoted"), b"dept:unrelated");
    assert!(
        engine.generation().dict.len() > dict_len_before,
        "the dictionary must have grown, or this test proves nothing"
    );

    assert!(
        !doubled.is_stale(&engine.generation()),
        "this session left no descriptor unresolved, duplicate or not"
    );
    assert_eq!(
        visible(&engine, &doubled),
        visible(&engine, &once),
        "and it still sees exactly what the single-descriptor credential sees"
    );
}

/// This session's total visible count over the whole extent.
fn visible(engine: &Engine, session: &Session) -> u64 {
    engine
        .viewport(
            session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize),
        )
        .expect("a viewport")
        .tiles
        .iter()
        .map(|tile| tile.visible)
        .sum()
}

/// The credential's own resolved descriptors: `satisfied` minus the reserved `public` term.
///
/// **Every session holds `public` by construction** (`per-point-attributes.md` §3.8), added inside
/// the engine rather than by the credential — so counting `satisfied` directly would count a term
/// this file's cases are not about, in every one of them.
fn resolved(session: &tessera_engine::Session) -> usize {
    assert!(
        session.satisfied.contains(&tessera_authz::PUBLIC_TERM),
        "every session holds the reserved `public` term"
    );
    session.satisfied.len() - 1
}
