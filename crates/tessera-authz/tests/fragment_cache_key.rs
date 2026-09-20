//! The fragment cache keys on the watermark it was built at (§9).

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use tessera_authz::{
    coalesce_delta_tiers, write_delta_tier, write_postings, DeltaTier, FragmentCache,
    PostingsReader,
};
use tessera_types::TermId;

const THRESHOLD: u32 = 32;

fn base(dir: &std::path::Path) -> PostingsReader {
    let path = dir.join("postings.arrow");
    write_postings(&path, &[vec![1, 2, 3]], THRESHOLD).unwrap();
    PostingsReader::open(&path, false).unwrap()
}

fn tier_path(dir: &std::path::Path, name: &str, entities: &[u32]) -> PathBuf {
    let path = dir.join(name);
    write_delta_tier(&path, &[(TermId::new(0), entities.to_vec())], THRESHOLD).unwrap();
    path
}

fn tier(dir: &std::path::Path, name: &str, entities: &[u32]) -> Arc<DeltaTier> {
    Arc::new(DeltaTier::open(&tier_path(dir, name, entities)).unwrap())
}

fn cache_at(dir: &TempDir, name: &str) -> FragmentCache {
    FragmentCache::new(&dir.path().join(name), [1u8; 32], [2u8; 32])
}

fn cache(dir: &TempDir) -> FragmentCache {
    cache_at(dir, "cache")
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

    let early = cache.get_or_build(&terms, &base, &[], 10).unwrap();
    let late = cache
        .get_or_build(
            &terms,
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
///
/// The tier under test is the **output of `coalesce_delta_tiers`**, not a hand-written stand-in,
/// so what this pins is the composition: the coalesce's encode and `build_fragment_with_deltas`'s
/// decode agreeing. A disagreement between them would surface only after a restart, as a session's
/// visible set changing with no flush behind it.
///
/// **The two builds must be two builds.** The single-flight map is keyed by the canonical key —
/// bundle identity, plugin hash, watermark and the sorted term ids, and nothing else — so a second
/// `get_or_build` on the same cache with the same terms and watermark returns the first fragment
/// from `Ready` whatever tiers it is handed, and would compare a value with itself. Two caches with
/// separate directories are what make the second build happen; the guard below is what keeps that
/// true if the keying changes.
///
/// Mutations this kills: a coalesce that drops an input, a term or an entity; a coalesce that
/// re-encodes a posting in a form the fragment build reads differently; and the vacuity itself —
/// collapsing the two caches back into one fails the guard rather than passing silently.
#[test]
fn coalescing_tiers_leaves_the_fragment_unchanged() {
    let dir = TempDir::new().unwrap();
    let base = base(dir.path());
    let terms = [TermId::new(0)];

    let t1 = tier_path(dir.path(), "t1.arrow", &[7]);
    let t2 = tier_path(dir.path(), "t2.arrow", &[9]);
    let merged = dir.path().join("merged.arrow");
    coalesce_delta_tiers(&[t1.clone(), t2.clone()], &merged, THRESHOLD).unwrap();

    let separate_cache = cache_at(&dir, "separate");
    let coalesced_cache = cache_at(&dir, "coalesced");
    assert_ne!(
        separate_cache.path_of(&terms, 20),
        coalesced_cache.path_of(&terms, 20),
        "the two builds must land in different cache entries, or the second is a Ready hit on \
         the first and this test compares a value with itself"
    );

    let separate = separate_cache
        .get_or_build(
            &terms,
            &base,
            &[
                Arc::new(DeltaTier::open(&t1).unwrap()),
                Arc::new(DeltaTier::open(&t2).unwrap()),
            ],
            20,
        )
        .unwrap();
    let coalesced = coalesced_cache
        .get_or_build(
            &terms,
            &base,
            &[Arc::new(DeltaTier::open(&merged).unwrap())],
            20,
        )
        .unwrap();

    // The tiers must contribute, or the equality would hold over the base alone.
    let separate_set = separate.view().to_vec();
    assert!(
        separate_set.contains(&7) && separate_set.contains(&9),
        "the delta tiers must reach the fragment, or this test proves nothing: {separate_set:?}"
    );
    assert_eq!(separate.view().to_vec(), coalesced.view().to_vec());
}

/// A cache reopens its own entries — the property that makes a persistent cache worth having.
#[test]
fn an_entry_survives_a_reopen_of_the_cache() {
    let dir = TempDir::new().unwrap();
    let base = base(dir.path());
    let terms = [TermId::new(0)];

    let built = cache(&dir)
        .get_or_build(&terms, &base, &[], 10)
        .unwrap()
        .view()
        .to_vec();

    let reopened = cache(&dir)
        .get_or_build(&terms, &base, &[], 10)
        .unwrap();
    assert_eq!(reopened.view().to_vec(), built);
    assert_eq!(reopened.watermark, 10);
}
