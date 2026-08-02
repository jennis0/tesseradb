//! The fragment cache keys on the watermark it was built at (§9).

use std::sync::Arc;

use tempfile::TempDir;
use tessera_authz::{
    write_delta_tier, write_postings, DeltaTier, FragmentCache, PostingsReader, FRAGMENT_FORMAT,
};
use tessera_types::TermId;

const THRESHOLD: u32 = 32;

fn base(dir: &std::path::Path) -> PostingsReader {
    let path = dir.join("postings.arrow");
    write_postings(&path, &[vec![1, 2, 3]], THRESHOLD).unwrap();
    PostingsReader::open(&path, false).unwrap()
}

fn tier(dir: &std::path::Path, name: &str, entities: &[u32]) -> Arc<DeltaTier> {
    let path = dir.join(name);
    write_delta_tier(&path, &[(TermId::new(0), entities.to_vec())], THRESHOLD).unwrap();
    Arc::new(DeltaTier::open(&path).unwrap())
}

fn cache(dir: &TempDir) -> FragmentCache {
    FragmentCache::new(
        &dir.path().join("cache"),
        [1u8; 32],
        [2u8; 32],
        FRAGMENT_FORMAT,
    )
}

/// **Two watermarks, two entries.** Without this the same key names two different fragments: after
/// a flush, one session's entry was built over the tiers that existed then and another's over the
/// tiers that exist now, and `tmp_sibling`'s "both writers wrote byte-identical content" argument
/// — the thing that makes a concurrent write-then-rename safe — stops holding.
///
/// A **disclosure risk, not merely a staleness one**: the two fragments differ by the entities the
/// newer tiers carry, and which of them a session gets would be decided by whoever wrote last.
#[test]
fn the_watermark_is_part_of_the_disk_key() {
    let dir = TempDir::new().unwrap();
    let cache = cache(&dir);
    let base = base(dir.path());
    let terms = [TermId::new(0)];
    let hash = [3u8; 32];

    let early = cache.get_or_build(&terms, hash, 1, &base, &[], 10).unwrap();
    let late = cache
        .get_or_build(
            &terms,
            hash,
            1,
            &base,
            &[tier(dir.path(), "t.arrow", &[9])],
            20,
        )
        .unwrap();

    assert_ne!(
        cache.path_of(&terms, 10),
        cache.path_of(&terms, 20),
        "one path for two contents is the collision"
    );
    assert_eq!(early.watermark, 10);
    assert_eq!(late.watermark, 20);
    assert!(
        late.view().contains(9) && !early.view().contains(9),
        "and the entries genuinely differ, so the collision would have mattered"
    );
}

/// **A merge does not need a key of its own**, and this is why the watermark suffices. A merge
/// coalesces tiers as a content-preserving re-encode (§5.2) — the same (term, entity) pairs — so
/// the fragment it would produce is identical, and reusing the pre-merge entry is correct rather
/// than merely tolerable.
#[test]
fn coalescing_tiers_leaves_the_fragment_unchanged() {
    let dir = TempDir::new().unwrap();
    let cache = cache(&dir);
    let base = base(dir.path());
    let terms = [TermId::new(0)];

    let separate = cache
        .get_or_build(
            &terms,
            [3u8; 32],
            1,
            &base,
            &[
                tier(dir.path(), "t1.arrow", &[7]),
                tier(dir.path(), "t2.arrow", &[9]),
            ],
            20,
        )
        .unwrap();
    let coalesced = cache
        .get_or_build(
            &terms,
            [4u8; 32],
            1,
            &base,
            &[tier(dir.path(), "merged.arrow", &[7, 9])],
            20,
        )
        .unwrap();

    assert_eq!(separate.view().to_vec(), coalesced.view().to_vec());
}

/// Every pre-upgrade entry becomes unreachable when the key's shape changes — a leak rather than a
/// fail-open, since new code can never read one, but nothing on that path deletes anything. So the
/// cache carries a format version and sweeps what the previous one left.
#[test]
fn a_previous_format_versions_entries_are_swept() {
    let dir = TempDir::new().unwrap();
    let cache_dir = dir.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    // What a pre-versioning cache left behind: a `.frag`/`.meta` pair at the top level.
    std::fs::write(cache_dir.join("deadbeef.frag"), b"stale").unwrap();
    std::fs::write(cache_dir.join("deadbeef.meta"), b"stale").unwrap();

    let cache = FragmentCache::new(&cache_dir, [1u8; 32], [2u8; 32], FRAGMENT_FORMAT);
    assert_eq!(cache.sweep_orphans().unwrap(), 2, "both halves of the pair");
    assert!(!cache_dir.join("deadbeef.frag").exists());

    // And a sweep of a cache holding only current-version entries removes nothing.
    let base = base(dir.path());
    cache
        .get_or_build(&[TermId::new(0)], [3u8; 32], 1, &base, &[], 10)
        .unwrap();
    assert_eq!(cache.sweep_orphans().unwrap(), 0);
}

/// A cache written by this version reopens its own entries — the property the sweep must not
/// break, and the one that makes a persistent cache worth having.
#[test]
fn an_entry_survives_a_reopen_of_the_cache() {
    let dir = TempDir::new().unwrap();
    let base = base(dir.path());
    let terms = [TermId::new(0)];

    let built = cache(&dir)
        .get_or_build(&terms, [3u8; 32], 1, &base, &[], 10)
        .unwrap()
        .view()
        .to_vec();

    let reopened = cache(&dir)
        .get_or_build(&terms, [3u8; 32], 1, &base, &[], 10)
        .unwrap();
    assert_eq!(reopened.view().to_vec(), built);
    assert_eq!(reopened.watermark, 10);
}
