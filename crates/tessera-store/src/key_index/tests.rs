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

    // Sequential keys share their top bits, so they are spread over their own range.
    let mut sequential: Vec<(u64, u32)> = (0..50_000u64).map(|k| (k, k as u32)).collect();
    sequential.shuffle(&mut rng);
    check_spill(&sequential, 64 << 10, 9000);

    // Keys past both ends of the range the first entries set, arriving after the first spill.
    let mut outliers = sequential.clone();
    outliers.extend([(0u64, 1u32), (u64::MAX, 2), (u64::MAX - 1, 3), (1 << 40, 4)]);
    outliers.extend((0..3000u64).map(|k| ((1 << 50) + k, 60_000 + k as u32)));
    check_spill(&outliers, 64 << 10, 9000);

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

/// A run of 8000 `u64` keys seven apart, entity `key / 7`: nine entry pages.
fn nine_pages(dir: &Path) -> PathBuf {
    let entries: Vec<(u64, u32)> = (0..8000u64).map(|k| (k * 7, k as u32)).collect();
    let path = write_sorted(dir, "r", &entries, u64::MAX)[0].path.clone();
    assert_eq!(first_keys(&path).len(), 9);
    path
}

/// The first key of every entry page of a `u64` run, from its page index.
fn first_keys(path: &Path) -> Vec<u64> {
    let bytes = std::fs::read(path).unwrap();
    let pages = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
    let index = (1 + pages) * PAGE_SIZE;
    (0..pages)
        .map(|p| u64::from_le_bytes(bytes[index + p * 8..index + p * 8 + 8].try_into().unwrap()))
        .collect()
}

/// Edit entry page `p` of the run at `path` and write its checksum to match, as a writer that
/// put wrong bytes in a page would.
fn rewrite_page(path: &Path, p: usize, edit: impl FnOnce(&mut [u8])) {
    let mut bytes = std::fs::read(path).unwrap();
    let page = &mut bytes[PAGE_SIZE * (1 + p)..PAGE_SIZE * (2 + p)];
    edit(page);
    let crc = crc32fast::hash(&page[..PAGE_SIZE - 4]);
    page[PAGE_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_flipped_byte_in_a_page_is_found_on_that_pages_first_read_and_by_the_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let in_page_3 = first_keys(&path)[3] + 7 * 5;
    // A byte of page 3's gaps, one of its entities, and one of its zeros before the checksum.
    for at in [
        PAGE_SIZE * 4 + 20,
        PAGE_SIZE * 4 + 2000,
        PAGE_SIZE * 4 + PAGE_SIZE - 5,
    ] {
        let copy = dir.path().join("copy.keys");
        std::fs::copy(&path, &copy).unwrap();
        flip(&copy, at);
        let run = open::<u64>(&copy);
        assert_eq!(run.get(0).unwrap(), vec![0], "other pages still answer");
        assert_eq!(part_of(run.get(in_page_3).unwrap_err()), RunPart::Page(3));
        assert_eq!(part_of(run.get(in_page_3).unwrap_err()), RunPart::Page(3));
        assert_eq!(part_of(verify_run(&copy).unwrap_err()), RunPart::Page(3));
        let scanned: Result<Vec<_>, _> = run.iter().collect();
        assert_eq!(part_of(scanned.unwrap_err()), RunPart::Page(3));
    }
}

/// A page's count, gap width and first key are covered by its checksum; these are pages written
/// wrongly with a checksum to match.
#[test]
fn a_page_whose_count_gap_width_or_first_key_is_wrong_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let in_page_3 = first_keys(&path)[3] + 7 * 5;
    let first = first_keys(&path)[3];
    // Each case writes its bytes at its offset in page 3.
    let cases: [(&str, usize, Vec<u8>); 7] = [
        ("no entries", 0, 0u16.to_le_bytes().to_vec()),
        ("more entries than fit", 0, 1000u16.to_le_bytes().to_vec()),
        ("one entry fewer", 0, 931u16.to_le_bytes().to_vec()),
        ("a gap width past the key's", 2, vec![65]),
        ("a narrower gap width", 2, vec![2]),
        ("a wider gap width", 2, vec![4]),
        ("another first key", 3, (first + 7).to_le_bytes().to_vec()),
    ];
    assert_eq!(all(&open::<u64>(&path)).len(), 8000);
    assert_eq!(
        (first_keys(&path)[4] - first_keys(&path)[3]) / 7,
        932,
        "page 3 holds 932 entries"
    );
    for (name, at, bytes) in cases {
        let copy = dir.path().join("copy.keys");
        std::fs::copy(&path, &copy).unwrap();
        rewrite_page(&copy, 3, |page| {
            page[at..at + bytes.len()].copy_from_slice(&bytes)
        });
        let run = open::<u64>(&copy);
        assert_eq!(run.get(0).unwrap(), vec![0], "{name}");
        assert_eq!(
            part_of(run.get(in_page_3).unwrap_err()),
            RunPart::Page(3),
            "{name}"
        );
        assert_eq!(
            part_of(verify_run(&copy).unwrap_err()),
            RunPart::Page(3),
            "{name}"
        );
    }
}

/// Damages one page of a run of `K` many times over, each time a random edit to its count, gap
/// width, first key or a gap byte with its checksum rewritten to match. Nothing panics, and a run
/// the verifier passes answers every lookup as its own entries do.
fn check_damage<K: Key>(key: impl Fn(u32) -> K, seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sorted(dir.path(), "r", &sample(3000, key), u64::MAX)[0]
        .path
        .clone();
    let bytes = std::fs::read(&path).unwrap();
    let pages = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
    let head = 3 + K::WIDTH;
    let mut rng = StdRng::seed_from_u64(seed);
    let copy = dir.path().join("copy.keys");
    let mut passed = 0;
    for _ in 0..300 {
        std::fs::copy(&path, &copy).unwrap();
        let p = rng.gen_range(0..pages);
        let edit = rng.gen_range(0..4);
        let mut r = StdRng::seed_from_u64(rng.gen());
        rewrite_page(&copy, p, |page| match edit {
            0 => {
                let n = u16::from_le_bytes([page[0], page[1]]);
                let count = if r.gen() {
                    r.gen()
                } else {
                    n.wrapping_add(r.gen_range(0..5)).wrapping_sub(2)
                };
                page[0..2].copy_from_slice(&count.to_le_bytes());
            }
            1 => page[2] = r.gen_range(0..=8 * K::WIDTH as u8 + 2),
            2 => page[3 + r.gen_range(0..K::WIDTH)] ^= 1 << r.gen_range(0..8),
            _ => {
                let (n, bits) = (
                    u16::from_le_bytes([page[0], page[1]]) as usize,
                    page[2] as usize,
                );
                let gap_bytes = ((n - 1) * bits).div_ceil(8);
                if gap_bytes > 0 {
                    page[head + r.gen_range(0..gap_bytes)] ^= 1 << r.gen_range(0..8);
                }
            }
        });
        let run = open::<K>(&copy);
        let scanned: Result<Vec<(K, u32)>, _> = run.iter().collect();
        let probes: Vec<K> = bytes_keys::<K>(&bytes, pages);
        let _ = run.lookup_sorted(&probes);
        for &k in &probes {
            let _ = run.get(k);
        }
        if verify_run(&copy).is_err() {
            continue;
        }
        passed += 1;
        let mut want: BTreeMap<K, Vec<u32>> = BTreeMap::new();
        for (k, e) in scanned.unwrap() {
            want.entry(k).or_default().push(e);
        }
        let run = open::<K>(&copy);
        let mut keys: Vec<K> = want.keys().copied().chain(probes).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut single = Vec::new();
        for (i, &k) in keys.iter().enumerate() {
            let got = run.get(k).unwrap();
            assert_eq!(got, want.get(&k).cloned().unwrap_or_default(), "{k:?}");
            single.extend(got.into_iter().map(|e| (i, e)));
        }
        assert_eq!(run.lookup_sorted(&keys).unwrap(), single);
    }
    assert!(passed > 0, "some edits leave a page the verifier passes");
}

/// The first key of every page of a run of `K`, from the page index of its bytes.
fn bytes_keys<K: Key>(bytes: &[u8], pages: usize) -> Vec<K> {
    let index = (1 + pages) * PAGE_SIZE;
    (0..pages)
        .map(|p| K::read(&bytes[index + p * K::WIDTH..]))
        .collect()
}

#[test]
fn a_page_damaged_at_random_never_panics_and_answers_as_it_scans_when_it_verifies() {
    check_damage::<u32>(|i| i * 7 + (i % 5) * (i % 11), 1);
    check_damage::<u64>(|i| ((i as u64) << 20) + (i as u64 % 97) * 5000, 2);
    check_damage::<u128>(|i| keyword_key(&format!("k{i}")), 3);
}

/// A gap that carries a key past the largest key of its width is refused, not wrapped.
#[test]
fn a_gap_past_the_largest_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sorted(dir.path(), "r", &[(1u32, 5), (u32::MAX, 6)], 10)[0]
        .path
        .clone();
    // Head: count, width 32, first key; then the one 32-bit gap.
    rewrite_page(&path, 0, |page| {
        page[7..11].copy_from_slice(&u32::MAX.to_le_bytes())
    });
    let run = open::<u32>(&path);
    assert_eq!(part_of(run.get(1).unwrap_err()), RunPart::Page(0));
    assert_eq!(part_of(verify_run(&path).unwrap_err()), RunPart::Page(0));
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
fn a_header_whose_entry_count_does_not_fit_its_pages_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let header = |entries: u64| Header {
        key_width: 8,
        entries,
        pages: 9,
        min_key: 0,
        max_key: 7 * 7999,
    };
    // Nine pages hold 9 to 9 × 1020 entries; 8001 fits them and is found wrong by a full read.
    for (entries, at_open) in [(8, true), (9 * 1020 + 1, true), (8001, false)] {
        let copy = dir.path().join("copy.keys");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..PAGE_SIZE].copy_from_slice(&header(entries).encode());
        std::fs::write(&copy, bytes).unwrap();
        if at_open {
            assert_eq!(
                part_of(KeyRun::<u64>::open(&copy).err().unwrap()),
                RunPart::Header
            );
        } else {
            let scanned: Result<Vec<_>, _> = open::<u64>(&copy).iter().collect();
            assert_eq!(part_of(scanned.unwrap_err()), RunPart::Header);
        }
        assert_eq!(part_of(verify_run(&copy).unwrap_err()), RunPart::Header);
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

/// The header page's unused bytes are covered by no lookup and the verifier requires them zero;
/// an entry page's unused bits and bytes are checked on its first read.
#[test]
fn non_zero_unused_bytes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    // 500 entries one key apart: one page, whose 499 one-bit gaps end three bits into byte 81 and
    // whose entities end at byte 2082.
    let entries: Vec<(u128, u32)> = (0..500u128).map(|k| (k, 0)).collect();
    let path = write_sorted(dir.path(), "r", &entries, 1000)[0]
        .path
        .clone();

    let copy = dir.path().join("header.keys");
    std::fs::copy(&path, &copy).unwrap();
    flip(&copy, 1000);
    assert_eq!(open::<u128>(&copy).get(3).unwrap(), vec![0]);
    assert_eq!(part_of(verify_run(&copy).unwrap_err()), RunPart::Header);

    for (name, at, value) in [
        ("after the entities", 3000, 1),
        ("after the gaps", 19 + 62, 0x80),
    ] {
        let copy = dir.path().join("page.keys");
        std::fs::copy(&path, &copy).unwrap();
        rewrite_page(&copy, 0, |page| page[at] |= value);
        assert_eq!(
            part_of(open::<u128>(&copy).get(450).unwrap_err()),
            RunPart::Page(0),
            "{name}"
        );
        assert_eq!(
            part_of(verify_run(&copy).unwrap_err()),
            RunPart::Page(0),
            "{name}"
        );
    }
}

/// Page checksums catch damage; the order checks catch a writer that put entries out of order
/// with correct checksums over them, once both neighbouring pages have been read.
#[test]
fn entries_out_of_order_across_pages_are_refused_by_lookups_and_the_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let path = nine_pages(dir.path());
    let firsts = first_keys(&path);
    // Page 1's last entry becomes page 2's first key with a larger entity. Its gap is wider than
    // the page's others, so every other one of its first sixty entries is dropped to make room.
    let mut page_1: Vec<(u64, u32)> = all(&open::<u64>(&path))
        .into_iter()
        .filter(|&(k, _)| firsts[1] <= k && k < firsts[2])
        .collect();
    let mut position = 0;
    page_1.retain(|_| {
        position += 1;
        position > 60 || position % 2 == 1
    });
    *page_1.last_mut().unwrap() = (firsts[2], u32::MAX);
    let mut builder = page::PageBuilder::<u64>::new();
    for &(k, e) in &page_1 {
        assert!(builder.try_push(k, e));
    }
    let mut sealed = vec![0u8; PAGE_SIZE];
    builder.seal(&mut sealed);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[PAGE_SIZE * 2..PAGE_SIZE * 3].copy_from_slice(&sealed);
    std::fs::write(&path, &bytes).unwrap();

    let run = open::<u64>(&path);
    assert_eq!(run.get(0).unwrap(), vec![0]);
    let in_page_2 = firsts[2] + 7 * 3;
    assert_eq!(run.get(in_page_2).unwrap(), vec![(in_page_2 / 7) as u32]);
    assert_eq!(
        part_of(run.get(firsts[1] + 7 * 100).unwrap_err()),
        RunPart::Page(1)
    );
    assert_eq!(part_of(verify_run(&path).unwrap_err()), RunPart::Page(2));
}

/// Keys `key(i)` for `i` in `0..n`, each held by one to three entities, written and read back:
/// every entry, every key's entities by single and batched lookups, a key between each two, and
/// the verifier.
fn check_keys<K: Key>(n: u32, key: impl Fn(u32) -> K, between: impl Fn(u32) -> Option<K>) {
    let dir = tempfile::tempdir().unwrap();
    let entries = sample(n, &key);
    let runs = write_sorted(dir.path(), "r", &entries, u64::MAX);
    let run = open::<K>(&runs[0].path);
    let mut expected = entries.clone();
    expected.sort_unstable();
    assert_eq!(all(&run), expected);
    verify_run(&runs[0].path).unwrap();
    let mut probes: Vec<K> = (0..n).map(&key).chain((0..n).filter_map(between)).collect();
    probes.sort_unstable();
    let want: BTreeMap<K, Vec<u32>> = expected.iter().fold(BTreeMap::new(), |mut m, &(k, e)| {
        m.entry(k).or_insert_with(Vec::new).push(e);
        m
    });
    let mut single = Vec::new();
    for (i, &k) in probes.iter().enumerate() {
        let got = run.get(k).unwrap();
        assert_eq!(got, want.get(&k).cloned().unwrap_or_default(), "{k:?}");
        single.extend(got.into_iter().map(|e| (i, e)));
    }
    assert_eq!(run.lookup_sorted(&probes).unwrap(), single);
}

#[test]
fn gaps_as_wide_as_the_key_round_trip() {
    check_keys::<u32>(4, |i| [0, 1, u32::MAX - 1, u32::MAX][i as usize], |_| None);
    check_keys::<u64>(4, |i| [0, 1, u64::MAX - 1, u64::MAX][i as usize], |_| None);
    check_keys::<u128>(
        4,
        |i| [0, 1, u128::MAX - 1, u128::MAX][i as usize],
        |_| None,
    );
    // A long run of the widest gaps a sorted run can hold many of.
    check_keys::<u32>(4000, |i| i << 20, |i| Some((i << 20) + 1));
    check_keys::<u64>(3000, |i| (i as u64) << 52, |i| Some(((i as u64) << 52) + 1));
    check_keys::<u64>(250, |i| (i as u64) << 56, |i| Some(((i as u64) << 56) + 1));
    check_keys::<u128>(
        3000,
        |i| (i as u128) << 116,
        |i| Some(((i as u128) << 116) + 1),
    );
}

#[test]
fn a_key_held_by_thousands_of_entities_round_trips() {
    fn one_key<K: Key>(key: K, other: K) {
        let dir = tempfile::tempdir().unwrap();
        let mut entries: Vec<(K, u32)> = (0..5000).map(|e| (key, e * 3)).collect();
        entries.push((other, 1));
        let runs = write_sorted(dir.path(), "r", &entries, u64::MAX);
        let run = open::<K>(&runs[0].path);
        let all_of_key: Vec<u32> = (0..5000).map(|e| e * 3).collect();
        assert_eq!(run.get(key).unwrap(), all_of_key);
        assert_eq!(run.get(other).unwrap(), vec![1]);
        let batched = run.lookup_sorted(&[key, key, other]).unwrap();
        assert_eq!(batched.len(), 10_001);
        assert!(batched[..5000].iter().all(|&(i, _)| i == 0));
        assert!(batched[5000..10_000].iter().all(|&(i, _)| i == 1));
        assert_eq!(batched[10_000], (2, 1));
        verify_run(&runs[0].path).unwrap();
    }
    one_key::<u32>(7, 9);
    one_key::<u64>(u64::MAX - 1, u64::MAX);
    one_key::<u128>(keyword_key("x"), u128::MAX);
}

/// Gaps that grow from one bit to sixty bits partway through pages, so a page's width rises as it
/// fills and some entries move to the next page.
#[test]
fn keys_whose_gaps_widen_partway_through_a_page_round_trip() {
    let mut keys = Vec::with_capacity(6000);
    let mut k = 0u64;
    for i in 0..6000u32 {
        keys.push(k);
        k += if i % 97 == 96 { 1 << (i / 97 % 60) } else { 2 };
    }
    check_keys::<u64>(6000, |i| keys[i as usize], |i| Some(keys[i as usize] + 1));
}

/// Record numbers pack at a few bits a gap; keys spread across their width pack no larger than
/// fixed-width entries.
#[test]
fn a_run_takes_less_space_the_denser_its_keys() {
    let dir = tempfile::tempdir().unwrap();
    let size = |stem: &str, entries: &[(u64, u32)]| {
        let runs = write_sorted(dir.path(), stem, entries, u64::MAX);
        std::fs::metadata(&runs[0].path).unwrap().len() as f64 / entries.len() as f64
    };
    let mut rng = StdRng::seed_from_u64(3);
    let dense: Vec<(u64, u32)> = (0..100_000u64)
        .map(|i| (2 * i + rng.gen_range(0..2), rng.gen()))
        .collect();
    let random: Vec<(u64, u32)> = (0..100_000).map(|_| (rng.gen(), rng.gen())).collect();
    assert!(size("dense", &dense) < 4.7);
    assert!(size("random", &random) < 12.0);
}

/// Runs of `K` written from `runs`, looked up through a view of them as live runs, a view of
/// their merge as base runs, and a view of both, answer as the set of pairs does; a batched lookup
/// answers as the single lookups do in each. The keys looked up are `probes`, and every stored key
/// and the keys one either side of it.
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

    let step = |k: K, by: u128| K::narrow(k.widen().wrapping_add(by));
    let mut keys: Vec<K> = probes.iter().map(|&k| key(k)).collect();
    keys.extend(
        model
            .iter()
            .flat_map(|&(k, _)| [k, step(k, 1), step(k, u128::MAX)]),
    );
    keys.sort_unstable();
    keys.dedup();
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

/// Keys from a small domain, so one key's entries span pages and runs, mixed with keys from the
/// whole of `u32`, so gaps range from zero to the key's width and a page's gap width grows as it
/// fills.
fn wide_runs_strategy() -> impl Strategy<Value = Vec<(Vec<(u32, u32)>, u64)>> {
    prop::collection::vec(
        (
            prop::collection::vec((prop_oneof![0u32..16, any::<u32>()], 0u32..3000), 0..1500),
            50u64..2000,
        ),
        1..4,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn u32_runs_of_spread_keys_answer_as_a_set_of_pairs(
        runs in wide_runs_strategy(),
        retired in prop::collection::vec(0u32..3000, 0..400),
        probes in prop::collection::vec(any::<u32>(), 1..40),
        merge_max in 50u64..800,
    ) {
        check_against_a_set_of_pairs::<u32>(
            |k| k.wrapping_mul(0x9E37_79B9),
            &runs,
            &retired,
            &probes,
            merge_max,
        )?;
    }

    #[test]
    fn u64_runs_of_spread_keys_answer_as_a_set_of_pairs(
        runs in wide_runs_strategy(),
        retired in prop::collection::vec(0u32..3000, 0..400),
        probes in prop::collection::vec(any::<u32>(), 1..40),
        merge_max in 50u64..800,
    ) {
        check_against_a_set_of_pairs::<u64>(
            |k| (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            &runs,
            &retired,
            &probes,
            merge_max,
        )?;
    }

    #[test]
    fn u128_runs_of_spread_keys_answer_as_a_set_of_pairs(
        runs in wide_runs_strategy(),
        retired in prop::collection::vec(0u32..3000, 0..400),
        probes in prop::collection::vec(any::<u32>(), 1..40),
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
