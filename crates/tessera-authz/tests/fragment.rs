//! Task 6: mask fragment build and the frozen cache. `build_fragment` is the authorise path
//! (measured as postings union) — tested here against a brute-force `HashSet<u32>` union.
//! `FragmentCache` is a directory-backed frozen-bitmap cache, keyed on
//! `bundle_identity ‖ auth_plugin_hash ‖ sorted term ids`, so a cache dir reused across bundle
//! rebuilds cannot serve a fragment naming a different entity set (a disclosure bug, not a perf
//! bug — Reference Sheet R4, brief §Task 6).

use std::collections::HashSet;

use rand::rngs::StdRng;
use rand::SeedableRng;
use tempfile::TempDir;

use tessera_authz::{build_fragment, write_postings, FragmentCache};
use tessera_types::TermId;

const UNIVERSE: u32 = 100_000;
const SMALL_TERM_THRESHOLD: u32 = 32;

/// Write postings for `term_count` random terms over `[0, UNIVERSE)`, returning the reader path
/// and the per-term entity sets (for brute-force comparison).
fn write_random_postings(
    dir: &std::path::Path,
    seed: u64,
    term_count: usize,
) -> (std::path::PathBuf, Vec<Vec<u32>>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut per_term = Vec::new();
    for _ in 0..term_count {
        // Mix of small (tag 0) and large (tag 1) postings so the union exercises both partitions.
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
        // Random grant subset of the 50 terms.
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
    let auth_data_hash = [1u8; 32];

    let mut expected: HashSet<u32> = HashSet::new();
    for t in &terms {
        expected.extend(per_term[t.raw() as usize].iter().copied());
    }

    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, auth_plugin_hash);
        assert_eq!(cache.rebuild_count(), 0);

        let frozen = cache
            .get_or_build(&terms, auth_data_hash, &reader, 42)
            .unwrap();
        assert_eq!(cache.rebuild_count(), 1);
        assert_eq!(frozen.watermark, 42);
        let got: HashSet<u32> = frozen.view().iter().collect();
        assert_eq!(got, expected);

        // Second call, same process, same key: must not rebuild.
        let frozen2 = cache
            .get_or_build(&terms, auth_data_hash, &reader, 42)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            1,
            "same-process repeat must hit the fast path"
        );
        assert_eq!(frozen2.watermark, 42);
    }

    // Drop the cache (and its in-memory map), reopen the cache dir fresh: the on-disk frozen
    // fragment must still be reused, not rebuilt.
    {
        let cache = FragmentCache::new(cache_dir.path(), bundle_identity, auth_plugin_hash);
        assert_eq!(cache.rebuild_count(), 0);

        let frozen = cache
            .get_or_build(&terms, auth_data_hash, &reader, 42)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            0,
            "reopening the cache dir must hit the on-disk frozen fragment, not rebuild"
        );
        let got: HashSet<u32> = frozen.view().iter().collect();
        assert_eq!(got, expected);

        // Frozen file length must equal the exact serialised size. Find the single `.frag` file
        // written under the cache dir rather than reaching into `FragmentCache`'s internals.
        let frag_path = std::fs::read_dir(cache_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("frag"))
            .expect("exactly one .frag file expected");
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
    let auth_data_hash = [1u8; 32];

    {
        let cache = FragmentCache::new(cache_dir.path(), [9u8; 32], auth_plugin_hash);
        cache
            .get_or_build(&terms, auth_data_hash, &reader, 1)
            .unwrap();
        assert_eq!(cache.rebuild_count(), 1);
    }

    // Same cache dir, different bundle_identity: must rebuild, not hit — a persistent cache dir
    // reused across bundle rebuilds must never serve a fragment naming a different entity set.
    {
        let cache = FragmentCache::new(cache_dir.path(), [10u8; 32], auth_plugin_hash);
        cache
            .get_or_build(&terms, auth_data_hash, &reader, 1)
            .unwrap();
        assert_eq!(
            cache.rebuild_count(),
            1,
            "different bundle_identity must not hit a fragment keyed under the old identity"
        );
    }
}
