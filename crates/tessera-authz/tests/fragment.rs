//! Mask fragment build and the frozen cache, tested against a brute-force `HashSet<u32>` union.
//! `FragmentCache` is keyed on `bundle_identity ‖ auth_plugin_hash ‖ sorted term ids`, so a cache
//! dir reused across bundle rebuilds cannot serve a fragment naming a different entity set.

use std::collections::HashSet;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::SeedableRng;
use tempfile::TempDir;

use tessera_authz::{build_fragment, write_postings, FragmentCache, FragmentCacheError};
use tessera_types::TermId;

const UNIVERSE: u32 = 100_000;

/// The single `.frag` file a cache has written, found by scanning its directory.
fn the_frag_file(cache_dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(cache_dir)
        .unwrap_or_else(|e| panic!("no cache dir at {}: {e}", cache_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("frag"))
        .expect("exactly one .frag file expected")
}
const SMALL_TERM_THRESHOLD: u32 = 32;

/// Write postings for `term_count` random terms over `[0, UNIVERSE)`, returning the reader path
/// and the per-term entity sets.
fn write_random_postings(
    dir: &std::path::Path,
    seed: u64,
    term_count: usize,
) -> (std::path::PathBuf, Vec<Vec<u32>>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut per_term = Vec::new();
    for _ in 0..term_count {
        let size = rand::Rng::gen_range(&mut rng, 0usize..500);
        let mut set: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        while set.len() < size {
            set.insert(rand::Rng::gen_range(&mut rng, 0..UNIVERSE));
        }
        per_term.push(set.into_iter().collect::<Vec<u32>>());
    }
    let path = dir.join("postings.arrow");
    write_postings(&path, &per_term, SMALL_TERM_THRESHOLD).unwrap();
    (path, per_term)
}

#[test]
fn build_fragment_matches_brute_force_union() {
    let temp = TempDir::new().unwrap();
    let (path, per_term) = write_random_postings(temp.path(), 1, 50);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let mut rng = StdRng::seed_from_u64(2);
    for _ in 0..20 {
        let grant: Vec<TermId> = (0..50u32)
            .filter(|_| rand::Rng::gen_bool(&mut rng, 0.3))
            .map(TermId::new)
            .collect();

        let fragment = build_fragment(&grant, &reader).unwrap();

        let mut expected: HashSet<u32> = HashSet::new();
        for t in &grant {
            expected.extend(per_term[t.raw() as usize].iter().copied());
        }

        let got: HashSet<u32> = fragment.iter().collect();
        assert_eq!(got, expected, "grant = {grant:?}");
    }
}

#[test]
fn build_fragment_empty_grant_is_empty() {
    let temp = TempDir::new().unwrap();
    let (path, _per_term) = write_random_postings(temp.path(), 3, 10);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let fragment = build_fragment(&[], &reader).unwrap();
    assert_eq!(fragment.cardinality(), 0);
}

#[test]
fn frozen_round_trip_second_call_does_not_rebuild() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, per_term) = write_random_postings(corpus_dir.path(), 7, 10);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let bundle_identity = [9u8; 32];
    let auth_plugin_hash = [7u8; 32];

    let terms: Vec<TermId> = (0..10u32).map(TermId::new).collect();

    let mut expected: HashSet<u32> = HashSet::new();
    for t in &terms {
        expected.extend(per_term[t.raw() as usize].iter().copied());
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, auth_plugin_hash);
        assert_eq!(cache.rebuild_count(), 0);

        let frozen = cache
            .get_or_build(&terms, &reader, &[], 42)
            .unwrap();
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(frozen.watermark, 42);
        let got: HashSet<u32> = frozen.view().iter().collect();
        assert_eq!(got, expected);

        let frozen2 = cache
            .get_or_build(&terms, &reader, &[], 42)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            1,
            "same-process repeat must hit the fast path"
        );
        assert_eq!(frozen2.watermark, 42);
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, auth_plugin_hash);
        assert_eq!(cache.rebuild_count(), 0);

        let frozen = cache
            .get_or_build(&terms, &reader, &[], 42)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            0,
            "reopening the cache dir must hit the on-disk frozen fragment, not rebuild"
        );
        assert_eq!(
            frozen.watermark, 42,
            "reopened fragment must restore its persisted watermark"
        );
        let got: HashSet<u32> = frozen.view().iter().collect();
        assert_eq!(got, expected);

        let frag_path = the_frag_file(cache_dir.path());
        let on_disk_len = std::fs::metadata(&frag_path).unwrap().len();
        let expected_len = frozen
            .view()
            .get_serialized_size_in_bytes::<croaring::Frozen>() as u64;
        assert_eq!(on_disk_len, expected_len);
    }
}

#[test]
fn stale_bundle_identity_misses_the_cache() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, _per_term) = write_random_postings(corpus_dir.path(), 11, 5);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let auth_plugin_hash = [7u8; 32];
    let terms: Vec<TermId> = (0..5u32).map(TermId::new).collect();

    {
        let cache = FragmentCache::new(cache_dir.path(), [9u8; 32], auth_plugin_hash);
        cache
            .get_or_build(&terms, &reader, &[], 1)
            .unwrap();
        assert_eq!(cache.rebuild_count(), 1);
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), [10u8; 32], auth_plugin_hash);
        cache
            .get_or_build(&terms, &reader, &[], 1)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            1,
            "different bundle_identity must not hit a fragment keyed under the old identity"
        );
    }
}

#[test]
fn stale_auth_plugin_hash_misses_the_cache() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, _per_term) = write_random_postings(corpus_dir.path(), 13, 5);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let bundle_identity = [9u8; 32];
    let terms: Vec<TermId> = (0..5u32).map(TermId::new).collect();

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, [7u8; 32]);
        cache
            .get_or_build(&terms, &reader, &[], 1)
            .unwrap();
        assert_eq!(cache.rebuild_count(), 1);
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, [8u8; 32]);
        cache
            .get_or_build(&terms, &reader, &[], 1)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            1,
            "different auth_plugin_hash must not hit a fragment keyed under the old plugin hash"
        );
    }
}

/// A bit-flipped `.frag` file must be treated as a cache miss and rebuilt, never a panic.
#[test]
fn bit_flipped_frag_file_is_treated_as_a_cache_miss_and_rebuilds() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, per_term) = write_random_postings(corpus_dir.path(), 21, 8);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let bundle_identity = [3u8; 32];
    let auth_plugin_hash = [4u8; 32];
    let terms: Vec<TermId> = (0..8u32).map(TermId::new).collect();

    let mut expected: HashSet<u32> = HashSet::new();
    for t in &terms {
        expected.extend(per_term[t.raw() as usize].iter().copied());
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, auth_plugin_hash);
        cache
            .get_or_build(&terms, &reader, &[], 5)
            .unwrap();
        assert_eq!(cache.rebuild_count(), 1);
    }

    let frag_path = the_frag_file(cache_dir.path());
    {
        let mut bytes = std::fs::read(&frag_path).unwrap();
        assert!(
            !bytes.is_empty(),
            "frag file must not be empty for this test to mean anything"
        );
        bytes[0] ^= 0xFF;
        std::fs::write(&frag_path, &bytes).unwrap();
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, auth_plugin_hash);
        let frozen = cache
            .get_or_build(&terms, &reader, &[], 5)
            .expect("a corrupted cache entry must fail closed to a rebuild, not an error");
        assert_eq!(
            cache.rebuild_count(),
            1,
            "a bit-flipped .frag file must be treated as a cache miss (rebuild), not a hit"
        );
        assert_eq!(frozen.watermark, 5);
        let got: HashSet<u32> = frozen.view().iter().collect();
        assert_eq!(
            got, expected,
            "the rebuilt fragment must still be the correct one"
        );
    }
}

/// Concurrent same-key misses must build exactly once, not once per thread.
#[test]
fn concurrent_cold_builds_single_flight_to_one_real_build() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, _per_term) = write_random_postings(corpus_dir.path(), 41, 20);
    let reader = Arc::new(tessera_authz::PostingsReader::open(&path, false).unwrap());

    let cache_dir = TempDir::new().unwrap();
    let cache = Arc::new(FragmentCache::new(cache_dir.path(), [1u8; 32], [2u8; 32]));
    let terms: Vec<TermId> = (0..20u32).map(TermId::new).collect();

    const THREADS: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let cache = Arc::clone(&cache);
            let reader = Arc::clone(&reader);
            let terms = terms.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let mut attempts = 0u32;
                loop {
                    attempts += 1;
                    assert!(
                        attempts < 1_000_000,
                        "get_or_build never converged out of Building -- looks like a D-G \
                         regression (a stuck waiter), not an ordinary race"
                    );
                    match cache.get_or_build(&terms, &reader, &[], 7) {
                        Ok(frozen) => return frozen,
                        Err(FragmentCacheError::Building) => {
                            std::thread::yield_now();
                            continue;
                        }
                        Err(e) => panic!("unexpected error: {e}"),
                    }
                }
            })
        })
        .collect();

    let results: Vec<Arc<tessera_authz::FrozenFragment>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in &results {
        assert!(
            Arc::ptr_eq(&results[0], r),
            "every thread must end up with the same Arc<FrozenFragment>, not independent builds"
        );
    }
    assert_eq!(
        cache.rebuild_count(),
        1,
        "lifecycle §3.3: concurrent same-key misses must build once, not once per racing thread"
    );
}

/// A warm hit must do no file IO at all, proven by deleting the on-disk `.frag`/`.meta` pair
/// after warm-up.
#[test]
fn warm_hit_does_no_file_io_after_backing_files_are_removed() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, per_term) = write_random_postings(corpus_dir.path(), 51, 6);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let cache = FragmentCache::new(cache_dir.path(), [3u8; 32], [4u8; 32]);
    let terms: Vec<TermId> = (0..6u32).map(TermId::new).collect();

    let mut expected: HashSet<u32> = HashSet::new();
    for t in &terms {
        expected.extend(per_term[t.raw() as usize].iter().copied());
    }

    let first = cache
        .get_or_build(&terms, &reader, &[], 11)
        .unwrap();
    assert_eq!(cache.rebuild_count(), 1);

    for entry in std::fs::read_dir(cache_dir.path()).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }

    let second = cache
        .get_or_build(&terms, &reader, &[], 11)
        .expect("a warm in-memory hit must succeed even with the backing files gone");
    assert!(
        Arc::ptr_eq(&first, &second),
        "warm hit must return the same in-memory Arc, not attempt to reopen (now-missing) disk \
         state"
    );
    assert_eq!(
        cache.rebuild_count(),
        1,
        "warm hit must not touch the filesystem at all, so it must not trigger a rebuild either"
    );
    let got: HashSet<u32> = second.view().iter().collect();
    assert_eq!(got, expected);
}

/// A build failure must never cache the error and must never leave a wedged `Building` entry.
#[test]
fn failed_build_leaves_no_wedge_and_retry_after_repair_succeeds() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, per_term) = write_random_postings(corpus_dir.path(), 61, 4);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let root = TempDir::new().unwrap();
    let blocker_path = root.path().join("blocker");
    std::fs::write(&blocker_path, b"not a directory").unwrap();
    let cache_dir = blocker_path.join("cache");

    let cache = FragmentCache::new(&cache_dir, [6u8; 32], [7u8; 32]);
    let terms: Vec<TermId> = (0..4u32).map(TermId::new).collect();

    let result = cache.get_or_build(&terms, &reader, &[], 1);
    assert!(
        matches!(result, Err(FragmentCacheError::Io(_))),
        "expected an Io error from a cache dir whose parent is a plain file, got {:?}",
        result.is_ok()
    );
    assert_eq!(
        cache.slot_count(),
        0,
        "a failed build must not leave a wedged Building entry, nor cache the Err (I13a)"
    );
    assert_eq!(cache.rebuild_count(), 0);

    std::fs::remove_file(&blocker_path).unwrap();
    std::fs::create_dir_all(&blocker_path).unwrap();

    let mut expected: HashSet<u32> = HashSet::new();
    for t in &terms {
        expected.extend(per_term[t.raw() as usize].iter().copied());
    }
    let frozen = cache
        .get_or_build(&terms, &reader, &[], 1)
        .expect("retry after repair must succeed");
    assert_eq!(cache.rebuild_count(), 1);
    let got: HashSet<u32> = frozen.view().iter().collect();
    assert_eq!(got, expected);
}

/// A rotation starts empty and carries the byte bound.
#[test]
fn a_rotation_starts_empty_and_keeps_the_byte_bound() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, _per_term) = write_random_postings(corpus_dir.path(), 17, 5);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let terms: Vec<TermId> = (0..5u32).map(TermId::new).collect();

    let cache = FragmentCache::new(cache_dir.path(), [9u8; 32], [7u8; 32]);
    cache.set_memory_bound(64 * 1024 * 1024);
    cache
        .get_or_build(&terms, &reader, &[], 1)
        .unwrap();
    assert_eq!(cache.rebuild_count(), 1);
    assert_eq!(cache.slot_count(), 1);
    let key_before = cache.canonical_key_for(&terms, 1);

    let rotated = cache.rotate([10u8; 32]);
    assert_eq!(rotated.slot_count(), 0, "no slot survives the identity");
    assert_eq!(
        rotated.stats().bound_bytes,
        64 * 1024 * 1024,
        "the validated bound is carried, or a rotation quietly unbounds the cache"
    );
    assert_ne!(rotated.canonical_key_for(&terms, 1), key_before);

    rotated
        .get_or_build(&terms, &reader, &[], 1)
        .unwrap();
    assert_eq!(
        rotated.rebuild_count(),
        1,
        "the rotated cache re-unioned the postings rather than reaching the pre-rotation entry"
    );
}

/// The identity a fragment reports is the one whose cache produced it.
#[test]
fn a_fragment_carries_the_identity_it_was_built_under() {
    let corpus_dir = TempDir::new().unwrap();
    let (path, _per_term) = write_random_postings(corpus_dir.path(), 19, 5);
    let reader = tessera_authz::PostingsReader::open(&path, false).unwrap();

    let cache_dir = TempDir::new().unwrap();
    let terms: Vec<TermId> = (0..5u32).map(TermId::new).collect();

    let cache = FragmentCache::new(cache_dir.path(), [9u8; 32], [7u8; 32]);
    let built = cache
        .get_or_build(&terms, &reader, &[], 1)
        .unwrap();
    assert_eq!(built.identity, [9u8; 32]);
    assert_eq!(cache.bundle_identity(), [9u8; 32]);

    let reopened = FragmentCache::new(cache_dir.path(), [9u8; 32], [7u8; 32])
        .get_or_build(&terms, &reader, &[], 1)
        .unwrap();
    assert_eq!(reopened.identity, [9u8; 32]);

    let rotated = cache.rotate([10u8; 32]);
    let rebuilt = rotated
        .get_or_build(&terms, &reader, &[], 1)
        .unwrap();
    assert_eq!(rebuilt.identity, [10u8; 32]);
}
