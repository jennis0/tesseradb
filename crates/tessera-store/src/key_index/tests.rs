//! The key index through its public surface: what a writer stores, what a reader answers, and
//! what a damaged file does.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use croaring::Bitmap;
use proptest::prelude::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use super::*;
use crate::error::StoreError;

fn write_sorted<K: Key>(
    dir: &Path,
    stem: &str,
    entries: &[(K, u32)],
    max: u64,
) -> Vec<WrittenRun<K>> {
    let mut sorted = entries.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut writer = KeyRunWriter::create(dir, stem, nz(max));
    for (k, e) in sorted {
        writer.push(k, e).unwrap();
    }
    writer.finish().unwrap()
}

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).unwrap()
}

fn open<K: Key>(path: &Path) -> KeyRun<K> {
    KeyRun::open(path).unwrap()
}

fn all<K: Key>(run: &KeyRun<K>) -> Vec<(K, u32)> {
    run.iter().collect::<Result<_, _>>().unwrap()
}

/// Entries with every key held by one to three entities, enough for several pages at any width.
fn sample<K: Key>(n: u32, key: impl Fn(u32) -> K) -> Vec<(K, u32)> {
    let mut out = Vec::new();
    for i in 0..n {
        for j in 0..(i % 3 + 1) {
            out.push((key(i), i * 10 + j));
        }
    }
    out
}

fn flip(path: &Path, at: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[at] ^= 0x10;
    std::fs::write(path, bytes).unwrap();
}

fn part_of(err: StoreError) -> RunPart {
    match err {
        StoreError::InvalidKeyIndex { part, .. } => part,
        other => panic!("expected a key index error, got {other:?}"),
    }
}

fn round_trip<K: Key>(key: impl Fn(u32) -> K) {
    let dir = tempfile::tempdir().unwrap();
    let entries = sample(1500, key);
    let runs = write_sorted(dir.path(), "r", &entries, u64::MAX);
    assert_eq!(runs.len(), 1);
    let run = open::<K>(&runs[0].path);
    let mut expected = entries.clone();
    expected.sort_unstable();
    assert_eq!(all(&run), expected);
    assert_eq!(run.len(), expected.len() as u64);
    assert_eq!(
        run.key_range(),
        Some((expected[0].0, expected.last().unwrap().0))
    );
    let check = verify_run(&runs[0].path).unwrap();
    assert_eq!(check.key_width, K::WIDTH);
    assert_eq!(check.entries, expected.len() as u64);
    assert!(check.pages > 3, "the sample spans several pages");
}

#[test]
fn every_key_width_round_trips() {
    round_trip::<u32>(|i| i * 7);
    round_trip::<u64>(|i| signed_key(i as i64 - 700));
    round_trip::<u128>(|i| keyword_key(&format!("value-{i}")));
}

#[test]
fn get_answers_present_absent_and_repeated_keys() {
    let dir = tempfile::tempdir().unwrap();
    // Key 500 is held by 700 entities, so its entries cross page boundaries.
    let mut entries: Vec<(u64, u32)> = (0..1000u64).map(|k| (k * 2, k as u32)).collect();
    entries.extend((0..700).map(|e| (500, 10_000 + e)));
    let runs = write_sorted(dir.path(), "r", &entries, u64::MAX);
    let run = open::<u64>(&runs[0].path);

    assert_eq!(run.get(10).unwrap(), vec![5]);
    assert_eq!(run.get(0).unwrap(), vec![0]);
    assert_eq!(run.get(1998).unwrap(), vec![999]);
    assert!(run.get(11).unwrap().is_empty(), "between keys");
    assert!(run.get(5000).unwrap().is_empty(), "past the largest key");
    let mut many = vec![250];
    many.extend(10_000..10_700);
    assert_eq!(run.get(500).unwrap(), many);
}

#[test]
fn a_batched_lookup_equals_single_lookups() {
    let dir = tempfile::tempdir().unwrap();
    let entries = sample::<u64>(3000, |i| (i as u64) * 3);
    let runs = write_sorted(dir.path(), "r", &entries, u64::MAX);
    let run = open::<u64>(&runs[0].path);
    let mut rng = StdRng::seed_from_u64(7);
    let mut keys: Vec<u64> = (0..2000).map(|_| rng.gen_range(0..9500)).collect();
    keys.extend([0, 0, 8997, 8997, 100_000]);
    keys.sort_unstable();
    let batched = run.lookup_sorted(&keys).unwrap();
    let single: Vec<(usize, u32)> = keys
        .iter()
        .enumerate()
        .flat_map(|(i, &k)| run.get(k).unwrap().into_iter().map(move |e| (i, e)))
        .collect();
    assert_eq!(batched, single);
}

#[test]
fn a_writer_splits_at_a_key_change_so_runs_have_disjoint_ranges() {
    let dir = tempfile::tempdir().unwrap();
    // Every key is held by three entities; a run of 100 entries would end inside a key.
    let entries: Vec<(u32, u32)> = (0..400u32)
        .flat_map(|k| (0..3).map(move |e| (k, e)))
        .collect();
    let runs = write_sorted(dir.path(), "r", &entries, 100);
    assert!(runs.len() > 5);
    assert!(runs.windows(2).all(|w| w[0].max_key < w[1].min_key));
    assert_eq!(runs.iter().map(|r| r.entries).sum::<u64>(), 1200);
    let mut back = Vec::new();
    for r in &runs {
        let run = open::<u32>(&r.path);
        assert_eq!(run.key_range(), Some((r.min_key, r.max_key)));
        back.extend(all(&run));
    }
    assert_eq!(back, entries);
}

#[test]
fn a_writer_refuses_entries_out_of_order_and_writes_nothing_for_no_entries() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = KeyRunWriter::<u64>::create(dir.path(), "r", nz(10));
    writer.push(5, 1).unwrap();
    assert!(writer.push(5, 1).is_err(), "a repeat");
    assert!(writer.push(4, 9).is_err(), "a smaller key");
    assert!(KeyRunWriter::<u64>::create(dir.path(), "none", nz(10))
        .finish()
        .unwrap()
        .is_empty());
}

#[test]
fn a_writer_never_overwrites_a_file_and_removes_what_it_wrote_when_dropped_unfinished() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("r-0.keys");
    std::fs::write(&existing, b"not a run").unwrap();
    let mut writer = KeyRunWriter::<u64>::create(dir.path(), "r", nz(10));
    assert!(writer.push(1, 1).is_err());
    drop(writer);
    assert_eq!(std::fs::read(&existing).unwrap(), b"not a run");

    let mut writer = KeyRunWriter::<u64>::create(dir.path(), "s", nz(10));
    for k in 0..25u64 {
        writer.push(k, 0).unwrap();
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 4);
    drop(writer);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "only the stranger's file"
    );
}

#[test]
fn a_one_entry_run_answers_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let runs = write_sorted(dir.path(), "r", &[(9u32, 4)], 10);
    let run = open::<u32>(&runs[0].path);
    assert_eq!(run.get(9).unwrap(), vec![4]);
    assert!(run.get(8).unwrap().is_empty());
    assert!(run.get(10).unwrap().is_empty());
    assert_eq!(run.lookup_sorted(&[8, 9, 9]).unwrap(), vec![(1, 4), (2, 4)]);
    assert_eq!(verify_run(&runs[0].path).unwrap().entries, 1);
}

#[test]
fn a_run_opened_for_another_key_width_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let runs = write_sorted(dir.path(), "r", &[(1u64, 1)], 10);
    assert_eq!(
        part_of(KeyRun::<u32>::open(&runs[0].path).err().unwrap()),
        RunPart::Header
    );
}

fn arc(path: &Path) -> Arc<KeyRun<u64>> {
    Arc::new(open(path))
}

#[test]
fn a_view_answers_live_runs_newest_first_then_the_one_base_run_holding_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let base_entries: Vec<(u64, u32)> = (0..3000u64).map(|k| (k, k as u32)).collect();
    let base = write_sorted(dir.path(), "base", &base_entries, 1000);
    assert_eq!(base.len(), 3);
    let older = write_sorted(dir.path(), "older", &[(5u64, 7000), (2500, 7001)], 100);
    let newer = write_sorted(dir.path(), "newer", &[(5u64, 8000), (9000, 8001)], 100);

    let view = KeyIndexView::new(
        vec![arc(&newer[0].path), arc(&older[0].path)],
        base.iter().map(|r| arc(&r.path)).collect(),
    )
    .unwrap();
    assert_eq!(
        view.get(5).unwrap(),
        vec![
            Found {
                run: RunRef::Live(0),
                entity: 8000
            },
            Found {
                run: RunRef::Live(1),
                entity: 7000
            },
            Found {
                run: RunRef::Base(0),
                entity: 5
            },
        ]
    );
    assert_eq!(
        view.get(2500).unwrap(),
        vec![
            Found {
                run: RunRef::Live(1),
                entity: 7001
            },
            Found {
                run: RunRef::Base(2),
                entity: 2500
            },
        ]
    );
    assert_eq!(
        view.get(9000).unwrap(),
        vec![Found {
            run: RunRef::Live(0),
            entity: 8001
        }]
    );
    assert!(view.get(4000).unwrap().is_empty());

    let keys = [5, 5, 1500, 2500, 4000, 9000];
    let batched = view.lookup_sorted(&keys).unwrap();
    let single: Vec<(usize, Found)> = keys
        .iter()
        .enumerate()
        .flat_map(|(i, &k)| view.get(k).unwrap().into_iter().map(move |f| (i, f)))
        .collect();
    assert_eq!(batched, single);
}

#[test]
fn a_view_lookup_reads_only_the_base_run_whose_range_holds_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let entries: Vec<(u64, u32)> = (0..3000u64).map(|k| (k, k as u32)).collect();
    let base = write_sorted(dir.path(), "base", &entries, 1000);
    // Damage the first entry page of the first and last base runs.
    flip(&base[0].path, PAGE_SIZE + 3);
    flip(&base[2].path, PAGE_SIZE + 3);
    let view = KeyIndexView::new(vec![], base.iter().map(|r| arc(&r.path)).collect()).unwrap();
    assert_eq!(
        view.get(1500).unwrap(),
        vec![Found {
            run: RunRef::Base(1),
            entity: 1500
        }]
    );
    assert_eq!(view.lookup_sorted(&[1200, 1500]).unwrap().len(), 2);
    assert!(view.get(0).is_err());
    assert!(view.get(2001).is_err());
}

#[test]
fn a_view_refuses_base_runs_that_overlap_or_are_out_of_order() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_sorted(dir.path(), "a", &[(1u64, 1), (10, 2)], 100);
    let b = write_sorted(dir.path(), "b", &[(10u64, 3), (20, 4)], 100);
    let c = write_sorted(dir.path(), "c", &[(30u64, 5)], 100);
    assert!(KeyIndexView::new(vec![], vec![arc(&a[0].path), arc(&b[0].path)]).is_err());
    assert!(KeyIndexView::new(vec![], vec![arc(&c[0].path), arc(&a[0].path)]).is_err());
    assert!(KeyIndexView::new(
        vec![arc(&b[0].path)],
        vec![arc(&a[0].path), arc(&c[0].path)]
    )
    .is_ok());
}

/// Runs a spill of `entries` under `budget` bytes and checks it against a sort in memory: the
/// entries written, the runs' disjoint ranges, the keys reported as duplicates, and the scratch
/// directory left empty.
fn check_spill<K: Key>(entries: &[(K, u32)], budget: usize, max: u64) {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(&out).unwrap();

    let mut spill = KeySpill::<K>::create(&scratch, budget).unwrap();
    for &(k, e) in entries {
        spill.push(k, e).unwrap();
    }
    let mut reported = BTreeMap::new();
    let runs = spill
        .finish(&out, "s", nz(max), |d| {
            let first = (d.first, d.entities);
            assert!(
                reported.insert(d.key, first).is_none(),
                "a key is reported once"
            );
        })
        .unwrap();

    let mut expected = entries.to_vec();
    expected.sort_unstable();
    expected.dedup();
    let mut per_key: BTreeMap<K, ([u32; 2], u64)> = BTreeMap::new();
    for &(k, e) in &expected {
        let (first, n) = per_key.entry(k).or_insert(([e, e], 0));
        if *n == 1 {
            first[1] = e;
        }
        *n += 1;
    }
    per_key.retain(|_, (_, n)| *n > 1);
    assert_eq!(
        reported, per_key,
        "exactly the keys held by more than one entity"
    );

    assert!(runs.windows(2).all(|w| w[0].max_key < w[1].min_key));
    let mut back = Vec::new();
    for r in &runs {
        verify_run(&r.path).unwrap();
        back.extend(all(&open::<K>(&r.path)));
    }
    assert_eq!(back, expected);
    assert_eq!(
        std::fs::read_dir(&scratch).unwrap().count(),
        0,
        "scratch is cleared"
    );
}

#[test]
fn a_spill_under_a_tiny_budget_equals_an_in_memory_sort() {
    let mut rng = StdRng::seed_from_u64(11);
    // Random keys with some repeated keys and some exact repeats.
    let mut random: Vec<(u64, u32)> = (0..50_000).map(|i| (rng.gen::<u64>(), i)).collect();
    let shared: Vec<(u64, u32)> = (0..500)
        .map(|i| (random[i * 7].0, 900_000 + i as u32))
        .collect();
    random.extend(shared);
    random.extend_from_within(0..300);
    random.shuffle(&mut rng);
    check_spill(&random, 64 << 10, 7000);

    // Sequential keys share their top bits, so every entry lands in one first-level bucket.
    let mut sequential: Vec<(u64, u32)> = (0..50_000u64).map(|k| (k, k as u32)).collect();
    sequential.shuffle(&mut rng);
    check_spill(&sequential, 64 << 10, 9000);

    // One key held by many entities is split by entity.
    let mut one_key: Vec<(u32, u32)> = (0..5000).map(|e| (42, e * 13)).collect();
    one_key.extend((0..100).map(|k| (k, 1)));
    one_key.shuffle(&mut rng);
    check_spill(&one_key, 2048, 1000);

    // The same entry many times over is kept once.
    check_spill(&vec![(7u64, 3); 3000], 1024, 100);

    // Keyword keys, and a budget the whole input fits in.
    let words: Vec<(u128, u32)> = (0..5000)
        .map(|i| (keyword_key(&format!("w{}", i % 4000)), i))
        .collect();
    check_spill(&words, 16 << 10, 1000);
    check_spill(&words, 64 << 20, 1000);
    check_spill::<u64>(&[], 4096, 10);
}

#[test]
fn a_merge_drops_exactly_the_retired_entities() {
    let dir = tempfile::tempdir().unwrap();
    let a: Vec<(u64, u32)> = (0..2000u64).map(|k| (k, k as u32)).collect();
    let b: Vec<(u64, u32)> = (1000..3000u64).map(|k| (k, 5000 + k as u32)).collect();
    // `c` repeats some of `a`'s entries exactly.
    let c: Vec<(u64, u32)> = (0..500u64).map(|k| (k * 4, (k * 4) as u32)).collect();
    let inputs: Vec<PathBuf> = [("a", &a), ("b", &b), ("c", &c)]
        .iter()
        .map(|(stem, e)| write_sorted(dir.path(), stem, e, u64::MAX)[0].path.clone())
        .collect();

    let mut model: BTreeSet<(u64, u32)> = a.iter().chain(&b).chain(&c).copied().collect();
    let out = dir.path().join("kept");
    std::fs::create_dir(&out).unwrap();
    let kept = merge_runs::<u64>(&inputs, |_, _| false, &out, "m", nz(1000)).unwrap();
    let back: Vec<(u64, u32)> = kept
        .iter()
        .flat_map(|r| all(&open::<u64>(&r.path)))
        .collect();
    assert_eq!(back, model.iter().copied().collect::<Vec<_>>());
    assert!(kept.windows(2).all(|w| w[0].max_key < w[1].min_key));

    let retired: Bitmap = (0..6000u32).filter(|e| e % 5 == 0).collect();
    model.retain(|&(_, e)| !retired.contains(e));
    let out = dir.path().join("folded");
    std::fs::create_dir(&out).unwrap();
    let folded =
        merge_runs::<u64>(&inputs, |_, e| retired.contains(e), &out, "m", nz(1000)).unwrap();
    let back: Vec<(u64, u32)> = folded
        .iter()
        .flat_map(|r| all(&open::<u64>(&r.path)))
        .collect();
    assert_eq!(back, model.iter().copied().collect::<Vec<_>>());

    let out = dir.path().join("none");
    std::fs::create_dir(&out).unwrap();
    assert!(merge_runs::<u64>(&inputs, |_, _| true, &out, "m", nz(1000))
        .unwrap()
        .is_empty());
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "no file");
}

/// An entity-to-number map is keyed by entity, so a fold drops its entries by key.
#[test]
fn a_merge_can_drop_entries_by_key() {
    let dir = tempfile::tempdir().unwrap();
    let pairs: Vec<(u32, u32)> = (0..3000u32).map(|entity| (entity, entity / 2)).collect();
    let input = write_sorted(dir.path(), "map", &pairs, 3000)[0]
        .path
        .clone();
    let removed: Bitmap = (0..3000u32).filter(|e| e % 3 == 0).collect();
    let out = dir.path().join("folded");
    std::fs::create_dir(&out).unwrap();
    let folded =
        merge_runs::<u32>(&[input], |k, _| removed.contains(k), &out, "m", nz(1000)).unwrap();
    let back: Vec<(u32, u32)> = folded
        .iter()
        .flat_map(|r| all(&open::<u32>(&r.path)))
        .collect();
    let want: Vec<(u32, u32)> = pairs
        .into_iter()
        .filter(|&(k, _)| !removed.contains(k))
        .collect();
    assert_eq!(back, want);
}

/// A run of 3000 `u64` keys: 9 entry pages of 341 entries.
fn nine_pages(dir: &Path) -> PathBuf {
    let entries: Vec<(u64, u32)> = (0..3000u64).map(|k| (k, k as u32)).collect();
    write_sorted(dir, "r", &entries, u64::MAX)[0].path.clone()
}

#[test]
fn a_flipped_byte_in_a_page_is_found_on_that_pages_first_read_and_by_the_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let per_page = entries_per_page(8) as u64;
    // A byte in an early entry of page 3, and one in its last entry.
    for at in [PAGE_SIZE * 4 + 100, PAGE_SIZE * 4 + PAGE_SIZE - 5] {
        let copy = dir.path().join("copy.keys");
        std::fs::copy(&path, &copy).unwrap();
        flip(&copy, at);
        let run = open::<u64>(&copy);
        assert_eq!(run.get(0).unwrap(), vec![0], "other pages still answer");
        assert_eq!(
            part_of(run.get(3 * per_page + 5).unwrap_err()),
            RunPart::Page(3)
        );
        assert_eq!(
            part_of(run.get(3 * per_page + 5).unwrap_err()),
            RunPart::Page(3)
        );
        assert_eq!(part_of(verify_run(&copy).unwrap_err()), RunPart::Page(3));
        let scanned: Result<Vec<_>, _> = run.iter().collect();
        assert_eq!(part_of(scanned.unwrap_err()), RunPart::Page(3));
    }
}

#[test]
fn a_flipped_byte_in_the_header_or_page_index_is_refused_at_open_and_by_the_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let cases = [
        (20, RunPart::Header),                            // the entry count
        (40, RunPart::Header),                            // the smallest key
        (PAGE_SIZE * 10 + 9, RunPart::PageIndex),         // page 1's first key
        (PAGE_SIZE * 10 + 9 * 8 + 1, RunPart::PageIndex), // the index checksum
    ];
    for (at, part) in cases {
        let copy = dir.path().join("copy.keys");
        std::fs::copy(&path, &copy).unwrap();
        flip(&copy, at);
        assert_eq!(
            part_of(KeyRun::<u64>::open(&copy).err().unwrap()),
            part,
            "byte {at}"
        );
        assert_eq!(part_of(verify_run(&copy).unwrap_err()), part, "byte {at}");
    }

    let bytes = std::fs::read(&path).unwrap();
    for (name, len) in [
        ("one-short", bytes.len() - 1),
        ("under-a-page", 100),
        ("empty", 0),
    ] {
        let copy = dir.path().join(name);
        std::fs::write(&copy, &bytes[..len]).unwrap();
        assert_eq!(
            part_of(KeyRun::<u64>::open(&copy).err().unwrap()),
            RunPart::Header,
            "{name}"
        );
        assert_eq!(
            part_of(verify_run(&copy).unwrap_err()),
            RunPart::Header,
            "{name}"
        );
    }
}

#[test]
fn the_verifier_refuses_a_key_width_the_format_does_not_have() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("width.keys");
    let header = Header {
        key_width: 5,
        entries: 0,
        pages: 0,
        min_key: 0,
        max_key: 0,
    };
    let mut bytes = header.encode();
    bytes.extend_from_slice(&crc32fast::hash(&[]).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(part_of(verify_run(&path).unwrap_err()), RunPart::Header);
}

/// Bytes the format leaves unused are covered by no lookup; the verifier requires them zero.
#[test]
fn the_verifier_refuses_non_zero_unused_bytes() {
    let dir = tempfile::tempdir().unwrap();
    // 500 entries of 20 bytes: page 2 holds 92 entries and is unused after byte 1840.
    let entries: Vec<(u128, u32)> = (0..500u128).map(|k| (k, 0)).collect();
    let path = write_sorted(dir.path(), "r", &entries, 1000)[0]
        .path
        .clone();

    let copy = dir.path().join("header.keys");
    std::fs::copy(&path, &copy).unwrap();
    flip(&copy, 1000);
    assert_eq!(open::<u128>(&copy).get(3).unwrap(), vec![0]);
    assert_eq!(part_of(verify_run(&copy).unwrap_err()), RunPart::Header);

    let copy = dir.path().join("page.keys");
    let mut bytes = std::fs::read(&path).unwrap();
    let page = PAGE_SIZE * 3..PAGE_SIZE * 4;
    bytes[page.start + 2000] = 1;
    let crc = crc32fast::hash(&bytes[page.start..page.end - 4]);
    bytes[page.end - 4..page.end].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&copy, &bytes).unwrap();
    assert_eq!(open::<u128>(&copy).get(450).unwrap(), vec![0]);
    assert_eq!(part_of(verify_run(&copy).unwrap_err()), RunPart::Page(2));
}

/// Page checksums catch damage; the order checks catch a writer that put entries out of order
/// with correct checksums over them, once both neighbouring pages have been read.
#[test]
fn entries_out_of_order_across_pages_are_refused_by_lookups_and_the_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let mut bytes = std::fs::read(&path).unwrap();
    // Page 1's last entry becomes page 2's first key with a larger entity, and page 1's checksum
    // is rewritten to match.
    let per = entries_per_page(8);
    let last_of_page_1 = PAGE_SIZE * 2 + (per - 1) * 12;
    let page_2_first_key =
        u64::from_le_bytes(bytes[PAGE_SIZE * 3..PAGE_SIZE * 3 + 8].try_into().unwrap());
    bytes[last_of_page_1..last_of_page_1 + 8].copy_from_slice(&page_2_first_key.to_le_bytes());
    bytes[last_of_page_1 + 8..last_of_page_1 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    let page = PAGE_SIZE * 2..PAGE_SIZE * 3;
    let crc = crc32fast::hash(&bytes[page.start..page.end - 4]);
    bytes[page.end - 4..page.end].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let run = open::<u64>(&path);
    assert_eq!(run.get(0).unwrap(), vec![0]);
    let page_2_key = 2 * per as u64 + 3;
    assert_eq!(run.get(page_2_key).unwrap(), vec![page_2_key as u32]);
    assert_eq!(
        part_of(run.get(per as u64 + 3).unwrap_err()),
        RunPart::Page(1)
    );
    assert_eq!(part_of(verify_run(&path).unwrap_err()), RunPart::Page(2));
}

/// Runs of `K` written from `runs`, looked up through a view of them as live runs, a view of
/// their merge as base runs, and a view of both, answer as the set of pairs does; a batched lookup
/// answers as the single lookups do in each.
fn check_against_a_set_of_pairs<K: Key>(
    key: impl Fn(u32) -> K,
    runs: &[(Vec<(u32, u32)>, u64)],
    retired: &[u32],
    probes: &[u32],
    merge_max: u64,
) -> std::result::Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let mut model: BTreeSet<(K, u32)> = BTreeSet::new();
    let mut live = Vec::new();
    let mut paths = Vec::new();
    for (i, (entries, max)) in runs.iter().enumerate() {
        let entries: Vec<(K, u32)> = entries.iter().map(|&(k, e)| (key(k), e)).collect();
        model.extend(entries.iter().copied());
        for w in write_sorted(dir.path(), &format!("in{i}"), &entries, *max) {
            paths.push(w.path.clone());
            live.push(Arc::new(open::<K>(&w.path)));
        }
    }
    let retired: Bitmap = retired.iter().copied().collect();
    let out = dir.path().join("merged");
    std::fs::create_dir(&out).unwrap();
    let merged =
        merge_runs::<K>(&paths, |_, e| retired.contains(e), &out, "m", nz(merge_max)).unwrap();
    let base: Vec<Arc<KeyRun<K>>> = merged.iter().map(|w| Arc::new(open(&w.path))).collect();
    let kept: BTreeSet<(K, u32)> = model
        .iter()
        .copied()
        .filter(|&(_, e)| !retired.contains(e))
        .collect();
    let back: Vec<(K, u32)> = base.iter().flat_map(|r| all(r)).collect();
    prop_assert_eq!(&back, &kept.iter().copied().collect::<Vec<_>>());

    let mut keys: Vec<K> = probes.iter().map(|&k| key(k)).collect();
    keys.sort_unstable();
    let views = [
        (KeyIndexView::new(live.clone(), vec![]).unwrap(), &model),
        (KeyIndexView::new(vec![], base.clone()).unwrap(), &kept),
        (KeyIndexView::new(live, base).unwrap(), &model),
    ];
    for (view, want) in &views {
        let single: Vec<(usize, Found)> = keys
            .iter()
            .enumerate()
            .flat_map(|(i, &k)| view.get(k).unwrap().into_iter().map(move |f| (i, f)))
            .collect();
        prop_assert_eq!(view.lookup_sorted(&keys).unwrap(), single);
        for &k in &keys {
            let got: BTreeSet<u32> = view.get(k).unwrap().iter().map(|f| f.entity).collect();
            let want: BTreeSet<u32> = want
                .range((k, 0)..=(k, u32::MAX))
                .map(|&(_, e)| e)
                .collect();
            prop_assert_eq!(got, want);
        }
    }
    Ok(())
}

fn runs_strategy() -> impl Strategy<Value = Vec<(Vec<(u32, u32)>, u64)>> {
    // Few keys and many entities, so one key's entries span pages and runs.
    prop::collection::vec(
        (
            prop::collection::vec((0u32..16, 0u32..3000), 0..1500),
            50u64..2000,
        ),
        1..4,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn u32_runs_views_and_merges_answer_as_a_set_of_pairs(
        runs in runs_strategy(),
        retired in prop::collection::vec(0u32..3000, 0..400),
        probes in prop::collection::vec(0u32..18, 1..40),
        merge_max in 50u64..800,
    ) {
        check_against_a_set_of_pairs::<u32>(|k| k * 3, &runs, &retired, &probes, merge_max)?;
    }

    #[test]
    fn u64_runs_views_and_merges_answer_as_a_set_of_pairs(
        runs in runs_strategy(),
        retired in prop::collection::vec(0u32..3000, 0..400),
        probes in prop::collection::vec(0u32..18, 1..40),
        merge_max in 50u64..800,
    ) {
        check_against_a_set_of_pairs::<u64>(
            |k| signed_key(k as i64 - 20),
            &runs,
            &retired,
            &probes,
            merge_max,
        )?;
    }

    #[test]
    fn u128_runs_views_and_merges_answer_as_a_set_of_pairs(
        runs in runs_strategy(),
        retired in prop::collection::vec(0u32..3000, 0..400),
        probes in prop::collection::vec(0u32..18, 1..40),
        merge_max in 50u64..800,
    ) {
        check_against_a_set_of_pairs::<u128>(
            |k| ((k as u128) << 100) | k as u128,
            &runs,
            &retired,
            &probes,
            merge_max,
        )?;
    }
}
