//! **The publication seam, widened to survive a prefix flip** — compaction §4's four gaps.
//!
//! These cases stand in for a fold by publishing a *second prefix* whose bytes are the first's,
//! under a `MANIFEST.json` that differs — so the bundle identity rotates, every file still
//! verifies, and the seam is exercised in exactly the shape a fold uses it: `CURRENT` flipped, then
//! [`Engine::publish_rotated_prefix_for_test`]. What the stand-in does not model is the fold's *content*
//! (dropped rows, rewritten postings, dropped keys); it models the identity rotation, which is the
//! half Rule F's safety hangs on.
//!
//! **The fold itself is in `tests/fold.rs`**, which runs the real thing. These stay because they
//! isolate the seam: a fold exercises it only along the one path its publication takes, where each
//! case here holds one gap open on its own.
//!
//! The four gaps, and the case that holds each open:
//!
//! 1. the identity and its fragment cache move onto the generation —
//!    `a_rotation_moves_the_prefix_the_postings_the_identity_and_the_sidecar_together`;
//! 2. a session's held fragment is valid only while the identity matches —
//!    `a_session_fragment_is_rebuilt_across_a_rotation_not_reused` and
//!    `a_drill_down_after_a_rotation_does_not_reuse_the_superseded_prefixs_fragment`;
//! 3. the signature carries postings, identity, cache and sidecar — case 1 again, on the far side;
//! 4. `prefix_dir` rotates — `a_deny_published_after_a_flip_writes_into_the_new_prefix`, which is
//!    the silent data-loss path and the only one here whose failure leaves no wrong *answer*, just
//!    an acked deny missing from the restore path.

mod common;

use std::path::Path;

use common::*;
use sha2::{Digest, Sha256};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::wal::ChangeOp;
use tessera_types::EntityId;

/// Copy `root/from` to `root/to`, give the copy a `MANIFEST.json` that differs, and flip `CURRENT`
/// onto it — the state a fold leaves behind at compaction §4 step 4.
///
/// **The files are byte-copies and their digests are carried verbatim**, so the new prefix verifies
/// in full under `open_bundle` — which is what lets the restart case here assert something. Only
/// `MANIFEST.json` differs, in `provenance`, which is free-form and load-bearing for nothing: it is
/// the smallest edit that rotates the digest `CURRENT` names, and the digest *is* the bundle
/// identity.
fn clone_prefix_and_flip(root: &Path, from: &str, to: &str) {
    copy_tree(&root.join(from), &root.join(to));

    let manifest_path = root.join(to).join("MANIFEST.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    value["provenance"] = serde_json::json!({ "folded_from": from });
    let bytes = serde_json::to_vec_pretty(&value).unwrap();
    std::fs::write(&manifest_path, &bytes).unwrap();

    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    std::fs::write(
        root.join("CURRENT"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "prefix": to,
            "manifest_digest": hex,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Block until the executor has published every deny window it owes a side-manifest for.
///
/// A deny publication is a write-then-rename into the live prefix, and it happens at drain close —
/// *after* the ack. A case that accepts a deny and then clones the prefix directory would race it,
/// and the failure is a copy of a `.tmp` name that no longer exists rather than anything to do with
/// what the case is testing. Settling on the counter is the deterministic wait; there is no
/// "publication owed" gauge to read directly, so this waits for the count to advance and then to
/// hold still.
fn settle_deny_publications(engine: &Engine) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut last = engine.write_executor_stats().overlay_publications;
    let mut stable_since = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let now = engine.write_executor_stats().overlay_publications;
        if now != last {
            last = now;
            stable_since = std::time::Instant::now();
        } else if stable_since.elapsed() >= std::time::Duration::from_millis(100) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the executor never stopped publishing deny state"
        );
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The `SEGMENTS-<n>.json` names present under one prefix's only partition, sorted.
fn side_manifests(root: &Path, prefix: &str) -> Vec<String> {
    let dir = root.join(prefix).join("partitions/default");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("SEGMENTS-") && n.ends_with(".json"))
        .collect();
    names.sort();
    names
}

fn visible(engine: &Engine, session: &tessera_engine::Session) -> u64 {
    engine
        .viewport(
            session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 10) as usize),
        )
        .unwrap()
        .tiles[0]
        .visible
}

/// A fixture bundle, and an engine over it with its executor running and the **background refresh
/// off**.
///
/// The refresh is off in every case here because it would produce the very entries these cases
/// assert are missing: a pass running after the rotation rebuilds each resident session's fragment
/// under the new identity, which is correct behaviour and would make a request-path check that
/// *failed* to notice the rotation indistinguishable from one that noticed. With it off, every
/// rebuild counted below was forced by the request path.
fn engine_over_fixture(tmp: &tempfile::TempDir, root: &Path, cache: &str, wal: &str) -> Engine {
    build_fixture(
        root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        root,
        &tmp.path().join(cache),
        &tmp.path().join(wal),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("engine should open against a freshly built bundle");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

/// **Gaps 1 and 3: one swap carries the prefix, the base postings, the bundle identity, the
/// fragment cache and the external-id sidecar.**
///
/// Each of the five is asserted through an observable a caller has, because none of the fields is
/// public: the prefix and `segments_version` off the generation; the identity through
/// `fragment_canonical_key`, which hashes it; the sidecar by resolving an external id, which after
/// the flip can only be answered by a sidecar opened against the *new* prefix's manifest; and the
/// postings and cache by the rows a session still sees, which would be empty had the term index
/// been dropped rather than rotated.
///
/// **Mutations this kills:** carrying the live postings forward on a rotation (the counts go to
/// zero only if the new prefix's postings differ — so instead the identity assertion carries it:
/// a rotation that left `fragments` alone leaves the canonical key unchanged); leaving the sidecar
/// on the engine (the resolve below would answer through the superseded prefix's files, which the
/// `provenance` edit does not disturb — see the reclamation note in the test body).
#[test]
fn a_rotation_moves_the_prefix_the_postings_the_identity_and_the_sidecar_together() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let satisfied: Vec<_> = session.satisfied.iter().copied().collect();
    let before = engine.generation();
    let key_before = engine.fragment_canonical_key(&satisfied);
    let baseline = visible(&engine, &session);
    let probe_entity = entity_of_source(&root, 7);
    let probe_external = engine.external_id_of(probe_entity).unwrap().unwrap();

    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect("a committed prefix publishes");

    let after = engine.generation();
    assert_eq!(after.prefix, "v00001");
    assert_eq!(after.segments_version, before.segments_version + 1);
    assert_ne!(
        engine.fragment_canonical_key(&satisfied),
        key_before,
        "the bundle identity must rotate with the prefix, or every fragment key survives a fold"
    );
    assert_eq!(
        after.dict.len(),
        before.dict.len(),
        "the dictionary is carried forward, never renumbered or shrunk (compaction §3, pass 4)"
    );

    // The sidecar was re-opened against the new prefix's manifest: the paths it holds are under
    // `v00001`, which is what makes reclaiming `v00000` safe. Resolving both ways is the check.
    assert_eq!(
        engine.external_id_of(probe_entity).unwrap().unwrap(),
        probe_external
    );
    assert_eq!(
        engine.resolve_external_id(&probe_external).unwrap(),
        Some(probe_entity)
    );

    // And the rotated term index still answers: a session established after the flip sees exactly
    // what one established before it saw.
    let fresh = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(visible(&engine, &fresh), baseline);
}

/// **Gap 2: a session's own fragment is rebuilt across a rotation, not reused** — compaction §12's
/// obligation 3, and the half `Session` holds outside `FragmentCache` altogether.
///
/// A fold advances no watermark, so `Engine::fragment_for`'s watermark test alone returns the
/// session's pre-fold fragment for ever: a mask still containing every entity the fold retired,
/// composed against post-fold geometry. The identity comparison is what refuses it, and it has to
/// be made here — at composition — because rotating the cache does not reach an `Arc` a session is
/// already holding.
///
/// **Mutations this kills:** dropping the `identity ==` conjunct from `fragment_for` (the rebuild
/// count stays 0); rotating `FragmentCache` by mutating `bundle_identity` in place rather than
/// starting fresh, which would leave the pre-rotation entry `Ready` under the memo's key and serve
/// it without a build (also 0).
#[test]
fn a_session_fragment_is_rebuilt_across_a_rotation_not_reused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let baseline = visible(&engine, &session);
    let before = engine.generation();

    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect("a committed prefix publishes");

    // The rotated cache starts its own count at zero, so this is a count of rebuilds *since* the
    // flip and not a delta against a number the pre-flip engine ran up.
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        0,
        "the rotation itself builds nothing"
    );

    assert_eq!(
        visible(&engine, &session),
        baseline,
        "the same session still sees the same items across the flip"
    );
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "and it saw them through a fragment rebuilt under the new identity, not the one it \
         authorised with — the watermark test alone cannot see a fold"
    );
}

/// **The pre-rotation fragment is unreachable: in the memo, in the persisted `.frag` files, and
/// across a restart.**
///
/// The three are one property reached three ways. `canonical_key` hashes the bundle identity, so a
/// rotated cache computes a different key for the same credential; the key *is* the filename, so
/// the pre-rotation `.frag` is never named again; and a restart derives the identity from `CURRENT`,
/// so it lands on the post-rotation key rather than the pre-rotation one even though the old file
/// is still sitting in the cache directory.
///
/// **The rebuild count is the memo assertion, and it is the one that matters.** `key_memo` maps
/// `(auth_data_hash, dict_len, watermark)` to a canonical key, and a fold moves none of those three
/// — so a rotation that reused the memo would hand back the *pre-rotation* key, find the
/// pre-rotation `.frag` still on disc, and return it with **no rebuild at all**. Asserting `1`
/// rather than `>= 1` is what makes that distinguishable.
#[test]
fn a_pre_rotation_fragment_is_unreachable_by_key_on_disc_and_across_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let cache_dir = tmp.path().join("cache");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let satisfied: Vec<_> = session.satisfied.iter().copied().collect();
    let key_before = engine.fragment_canonical_key(&satisfied);
    let frag_before = cache_dir.join(format!("{}.frag", hex(&key_before)));
    assert!(
        frag_before.exists(),
        "the pre-rotation fragment is persisted, which is what makes this test's premise real"
    );

    let before = engine.generation();
    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect("a committed prefix publishes");

    let key_after = engine.fragment_canonical_key(&satisfied);
    assert_ne!(key_after, key_before);

    engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "an authorise after the rotation must re-union the postings, not find the pre-rotation \
         entry — which is exactly what a carried-over `key_memo` would make it do"
    );
    assert!(
        cache_dir.join(format!("{}.frag", hex(&key_after))).exists(),
        "and it persists under the new identity's key"
    );
    assert!(
        !frag_before.exists(),
        "the pre-rotation entry is swept at the flip (compaction §8): at that instant every \
         persisted entry is under the superseded identity, which is what makes 'everything \
         present' the exact set — a cache entry is a SHA-256 over the identity, so the set is not \
         selectable by name"
    );

    // Across a restart: the identity comes from `CURRENT`, so a fresh process over the same cache
    // directory lands on the post-rotation key. Nothing names the old file, and the `.frag` sitting
    // beside it is inert.
    drop(engine);
    let restarted = Engine::open(
        &root,
        &cache_dir,
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the new prefix opens on its own");
    let restarted_session = restarted.authorise(&full_coverage_credential()).unwrap();
    let restarted_satisfied: Vec<_> = restarted_session.satisfied.iter().copied().collect();
    assert_eq!(
        restarted.fragment_canonical_key(&restarted_satisfied),
        key_after,
        "a restart keys on the prefix CURRENT names, so the pre-rotation entry stays unreachable"
    );
}

/// **Gap 4: a deny published after a flip writes its side-manifest into the *new* prefix.**
///
/// The one failure here that produces no wrong answer: the deny is in force, the ack is honest, the
/// WAL is durable — and the `SEGMENTS-<n>.json` carrying it lands in the prefix reclamation is
/// about to delete, so a restore from bundle and object store recovers a node that never heard of
/// it. Nothing logs, nothing refuses. Asserted on the filesystem for that reason.
///
/// **Mutations this kills:** restoring `Executor::prefix_dir` as a stored field (the manifest lands
/// under `v00000`); deriving it from anything other than the generation being published against.
#[test]
fn a_deny_published_after_a_flip_writes_into_the_new_prefix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let old_before = side_manifests(&root, "v00000");
    let before = engine.generation();

    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect("a committed prefix publishes");
    let new_before = side_manifests(&root, "v00001");

    let publications_before = engine.write_executor_stats().overlay_publications;
    engine
        .accept_change(entity_of_source(&root, 11), ChangeOp::Suppress)
        .expect("a suppression is accepted");

    // The publication happens at drain close on the executor, one loop iteration after the ack.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while engine.write_executor_stats().overlay_publications == publications_before {
        assert!(
            std::time::Instant::now() < deadline,
            "the executor never published the deny state"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        side_manifests(&root, "v00000"),
        old_before,
        "the superseded prefix gains nothing: a side-manifest written there is acked deny state \
         that vanishes with the prefix, with no error anywhere"
    );
    assert!(
        side_manifests(&root, "v00001").len() > new_before.len(),
        "and the live prefix gained the manifest carrying it: {:?} was {:?}",
        side_manifests(&root, "v00001"),
        new_before
    );
}

/// **A drill-down after a rotation does not answer from the superseded prefix's fragment.**
///
/// `Engine::item` takes its fragment from the freshest *resident* projection entry rather than
/// rebuilding per request, and the retention depth deliberately keeps the generation immediately
/// below the live one — which, across a flip, is a pre-rotation entry holding a pre-rotation
/// fragment. Taking the max by `segments_version` alone would find it. The scoping is at the read,
/// in `RowProjectionCache::freshest_fragment`.
///
/// **Mutation this kills:** dropping the `key.prefix == prefix` filter — the resident pre-rotation
/// entry is then returned and no rebuild happens.
#[test]
fn a_drill_down_after_a_rotation_does_not_reuse_the_superseded_prefixs_fragment() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    // Populates the projection cache under `v00000`, carrying this session's fragment.
    visible(&engine, &session);

    let entity = entity_of_source(&root, 5);
    let id = engine.tessera_id_of(entity).unwrap();
    assert!(engine.item(&session, id, None).unwrap().is_some());

    let before = engine.generation();
    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect("a committed prefix publishes");

    assert_eq!(engine.fragment_cache_rebuilds(), 0);
    assert!(
        engine.item(&session, id, None).unwrap().is_some(),
        "the item is still there — the rotation preserved the data"
    );
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        1,
        "but the answer came from a fragment built under the new identity: the resident \
         pre-rotation entry is still in the projection cache and must not have been consulted"
    );
}

/// **Rule F rides the rotation: the executed deletions leave `deleted` in the swap, and a
/// suppression beside them does not** (write-path §5.4).
///
/// The seam's half of retirement — [`Engine::publish_rotated_prefix_for_test`]'s `retired` argument, the
/// `Overlay::retire` it reaches, and the deny mask re-derived over the result. **Deriving
/// `executed` is not this task's and is not modelled here**: compaction §5's rule is
/// `{ e ∈ D₀ : no carried-forward artefact names e }`, evaluated against what a publication
/// demonstrably removed, and the fold is what will evaluate it.
///
/// **The deleted item becomes visible again, and that is the point of the assertion rather than an
/// accident.** This stand-in prefix is a *copy*: it still holds the deleted entity's row and its
/// postings, so retiring the tombstone withdraws the only thing hiding it. A real fold retires only
/// entities whose row and postings it removed, which is why that rule exists and why it is stated
/// at every site that can reach this. What the line pins here is that retirement genuinely reaches
/// the composed mask — a version that dropped the entry from the overlay but left `denied` derived
/// over the pre-retirement set would keep the item hidden and look correct.
///
/// **Mutations this kills:** retiring nothing (depth stays 2); retiring the suppression too (depth
/// goes to 0 and the suppressed item reappears — Rule S's fail-open, the one caught twice in
/// review); deriving `denied` from `previous.overlay` rather than the retired one.
#[test]
fn a_rotation_retires_the_deletions_it_is_given_and_leaves_suppressions_alone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let deleted = entity_of_source(&root, 4);
    let suppressed = entity_of_source(&root, 8);
    let baseline = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    assert_eq!(engine.overlay_depth(), 2);
    settle_deny_publications(&engine);

    let before = engine.generation();
    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[deleted],
        )
        .expect("a committed prefix publishes");

    assert_eq!(
        engine.overlay_depth(),
        1,
        "the executed deletion left `deleted`; the suppression did not — Rule S gives a \
         suppression no retirement route at all"
    );

    let after = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &after),
        baseline - 1,
        "the suppressed item is still hidden and the retired one is not: retirement reached the \
         row-space deny mask, which is re-derived over the retired overlay"
    );
}

/// **A rotation that retires nothing changes no deny state**, which is the case every fold with a
/// fully carried-forward tombstone set produces — fail-closed, and the next fold takes them.
#[test]
fn a_rotation_with_an_empty_retirement_set_retires_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let deleted = entity_of_source(&root, 4);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    assert_eq!(engine.overlay_depth(), 1);
    settle_deny_publications(&engine);

    let before = engine.generation();
    clone_prefix_and_flip(&root, "v00000", "v00001");
    engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect("a committed prefix publishes");

    assert_eq!(
        engine.overlay_depth(),
        1,
        "an un-retired tombstone stands: a fold retires only what it demonstrably removed"
    );
}

/// **A prefix `CURRENT` does not name is refused, not published.**
///
/// `CURRENT` is the commit point and the bundle identity is the digest it names, so publishing an
/// uncommitted prefix leaves the process serving geometry a restart cannot find — a disagreement
/// with nothing to detect it until that restart.
#[test]
fn publishing_a_prefix_current_does_not_name_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(&tmp, &root, "cache", "wal.log");

    let before = engine.generation();
    // The prefix exists and is complete; what has not happened is the flip.
    copy_tree(&root.join("v00000"), &root.join("v00001"));

    let refused = engine
        .publish_rotated_prefix_for_test(
            "v00001",
            before.segments_version + 1,
            before.watermark,
            std::sync::Arc::clone(&before.dict),
            Vec::new(),
            &[],
        )
        .expect_err("CURRENT still names v00000");
    assert!(
        matches!(
            refused,
            tessera_engine::PublishGeometryError::PrefixNotCommitted { .. }
        ),
        "{refused:?}"
    );
    assert_eq!(
        engine.generation().prefix,
        before.prefix,
        "and nothing was swapped"
    );
}

/// The entity id `tessera-build` assigned to fixture source row `source_id`.
///
/// Resolved **before** any flip in every case here: `source_to_new_map` opens the bundle through
/// `CURRENT`, so calling it afterwards would be answering from the new prefix — correct, but it
/// would make the fixture depend on the thing under test.
fn entity_of_source(root: &Path, source_id: u64) -> EntityId {
    EntityId::new(source_to_new_map(root, "v00000")[&source_id])
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
