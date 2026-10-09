//! The sorted dictionary's format, from outside the crate — the surface three tracks will code
//! against (`records-and-search.md` §4.3).
//!
//! The footer is parsed here independently of the reader, so a layout change has to be made twice
//! to pass: once in `dict.rs` and once in [`Layout::of`]. That redundancy is the point — a test
//! that asked the reader where the restart table is could not tell a moved table from a moved
//! reader, and the fail-closed cases below all work by damaging a specific byte.

use std::io::Write;

use tessera_filter::{
    write_sorted_dict, Access, DictError, KeyMatcher, SortedDict, SortedDictWriter, DICT_FILE,
    DICT_FORMAT_VERSION,
};

/// Build a dictionary in memory over `keys` at an explicit restart interval.
fn build(keys: &[&str], restart_interval: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let stats = {
        let mut writer =
            SortedDictWriter::with_restart_interval(&mut out, restart_interval).unwrap();
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(
                writer.push(key).unwrap(),
                i as u32,
                "ordinals are positions"
            );
        }
        writer.finish().unwrap()
    };
    assert_eq!(stats.keys, keys.len() as u32);
    assert_eq!(
        stats.bytes,
        out.len() as u64,
        "the stats account for the file"
    );
    out
}

fn open(keys: &[&str], restart_interval: u32) -> SortedDict {
    SortedDict::from_vec(build(keys, restart_interval)).unwrap()
}

/// The footer, re-derived from the raw bytes rather than asked of the reader.
struct Layout {
    blocks_at: usize,
    blocks_len: usize,
    restarts_at: usize,
    block_count: usize,
    footer_at: usize,
}

impl Layout {
    fn of(bytes: &[u8]) -> Self {
        // version | K | keys | blocks | blocks_len | MAGIC = 4 + 4 + 4 + 4 + 8 + 4.
        let footer_at = bytes.len() - 28;
        let read32 = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let block_count = read32(footer_at + 12) as usize;
        let blocks_len =
            u64::from_le_bytes(bytes[footer_at + 16..footer_at + 24].try_into().unwrap()) as usize;
        Layout {
            blocks_at: 4,
            blocks_len,
            restarts_at: 4 + blocks_len,
            block_count,
            footer_at,
        }
    }
}

fn keys_of(dict: &SortedDict) -> Vec<String> {
    let mut out = Vec::new();
    dict.walk(|ordinal, key| {
        assert_eq!(ordinal, out.len() as u32, "the walk is in ordinal order");
        out.push(key.to_string());
    })
    .unwrap();
    out
}

fn malformed(result: Result<impl std::fmt::Debug, DictError>) -> String {
    match result {
        Err(DictError::Malformed(detail)) => detail,
        other => panic!("expected a Malformed refusal, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Round trip
// -------------------------------------------------------------------------------------------

/// The four key shapes the design names: identifiers with heavy shared prefixes (arXiv's), keys
/// with none at all (the prefix-free case §4.3 marks as the layout's worst), unicode, and keys long
/// enough that their lengths need a second varint byte.
#[test]
fn round_trip_over_every_key_shape() {
    let long = "z".repeat(300);
    let longer = format!("z{}", "z".repeat(300));
    let shapes: Vec<(&str, Vec<String>)> = vec![
        (
            "shared prefixes",
            vec![
                "0704.0001",
                "0704.0002",
                "0704.0010",
                "0704.1000",
                "0801.0001",
                "2412.9999",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        ),
        (
            "no shared prefixes",
            vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        (
            "unicode",
            // Sorted by bytes, which for UTF-8 is code-point order. "e" + U+0301 (NFD) and U+00E9
            // (NFC) are two distinct keys: §4.3 keeps byte-exact semantics, so nothing normalises.
            {
                let mut v = vec![
                    "e\u{0301}clair".to_string(),
                    "\u{e9}clair".to_string(),
                    "\u{e9}t\u{e9}".to_string(),
                    "\u{ea}tre".to_string(),
                    "\u{4e2d}\u{6587}".to_string(),
                    "\u{1f600}".to_string(),
                ];
                v.sort();
                v
            },
        ),
        ("long keys", vec![long.clone(), longer.clone()]),
    ];

    for (label, keys) in shapes {
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        for interval in [1, 2, 3, 16, 64] {
            let dict = open(&refs, interval);
            assert_eq!(dict.len(), keys.len() as u32, "{label} at K={interval}");
            assert_eq!(keys_of(&dict), keys, "{label} at K={interval}");
            dict.self_check().unwrap();
            for (i, key) in keys.iter().enumerate() {
                assert_eq!(
                    dict.resolve(key).unwrap(),
                    Some(i as u32),
                    "{label} at K={interval}: {key:?}"
                );
                let mut scratch = Vec::new();
                assert_eq!(dict.key_of(i as u32, &mut scratch).unwrap(), key);
            }
        }
    }
}

/// A shared prefix may end in the middle of a multi-byte code point — U+00E9 and U+00EA share
/// their leading 0xC3 — so a suffix on its own is not necessarily valid UTF-8. Only the
/// reassembled key is.
#[test]
fn a_shared_prefix_may_split_a_code_point() {
    let keys = ["\u{e9}", "\u{ea}", "\u{eb}"];
    let dict = open(&keys, 16);
    assert_eq!(keys_of(&dict), keys);
    assert_eq!(dict.resolve("\u{ea}").unwrap(), Some(1));
}

#[test]
fn an_empty_dictionary_is_legal_and_answers_nothing() {
    let dict = open(&[], 16);
    assert!(dict.is_empty());
    assert_eq!(dict.len(), 0);
    assert_eq!(dict.block_count(), 0);
    assert_eq!(dict.resolve("anything").unwrap(), None);
    assert_eq!(dict.prefix_range("").unwrap(), 0..0);
    assert_eq!(dict.prefix_range("a").unwrap(), 0..0);
    assert_eq!(keys_of(&dict), Vec::<String>::new());
    dict.self_check().unwrap();
    assert!(matches!(
        dict.key_of(0, &mut Vec::new()),
        Err(DictError::OrdinalOutOfRange { ordinal: 0, len: 0 })
    ));
}

// -------------------------------------------------------------------------------------------
// The operations
// -------------------------------------------------------------------------------------------

/// Even-numbered keys only, so every odd one is a miss that falls *between* two keys — and with a
/// restart interval of 4 over 40 keys there are ten blocks, so misses land either side of every
/// block boundary as well as inside blocks.
#[test]
fn resolve_hits_and_misses_either_side_of_every_block_boundary() {
    let present: Vec<String> = (0..40).map(|i| format!("k{:04}", i * 2)).collect();
    let refs: Vec<&str> = present.iter().map(String::as_str).collect();
    for interval in [1, 2, 4, 7, 16] {
        let dict = open(&refs, interval);
        for (i, key) in present.iter().enumerate() {
            assert_eq!(dict.resolve(key).unwrap(), Some(i as u32), "K={interval}");
        }
        for i in 0..40 {
            let absent = format!("k{:04}", i * 2 + 1);
            assert_eq!(dict.resolve(&absent).unwrap(), None, "K={interval}");
        }
        // Outside the dictionary at both ends, and a needle that is a strict prefix of a key.
        assert_eq!(dict.resolve("").unwrap(), None);
        assert_eq!(dict.resolve("a").unwrap(), None);
        assert_eq!(dict.resolve("zzzz").unwrap(), None);
        assert_eq!(dict.resolve("k00").unwrap(), None);
        assert_eq!(dict.resolve("k0000x").unwrap(), None);
    }
}

#[test]
fn prefix_range_over_every_shape() {
    // Blocks of two, so "ab" spans blocks and "b" sits inside one.
    let keys = ["aa", "ab", "abc", "abd", "abz", "b", "bc", "c"];
    for interval in [1, 2, 3, 8, 64] {
        let dict = open(&keys, interval);

        // Spans blocks, and the prefix is itself a key: "ab" covers ordinals 1..5.
        assert_eq!(dict.prefix_range("ab").unwrap(), 1..5, "K={interval}");
        // The whole dictionary.
        assert_eq!(dict.prefix_range("").unwrap(), 0..8, "K={interval}");
        // A prefix of everything under "a".
        assert_eq!(dict.prefix_range("a").unwrap(), 0..5, "K={interval}");
        // Exactly one key.
        assert_eq!(dict.prefix_range("abc").unwrap(), 2..3, "K={interval}");
        assert_eq!(dict.prefix_range("c").unwrap(), 7..8, "K={interval}");
        // Empty: before everything, after everything, and between two keys.
        assert_eq!(dict.prefix_range("A").unwrap(), 0..0, "K={interval}");
        assert_eq!(dict.prefix_range("zz").unwrap(), 8..8, "K={interval}");
        assert_eq!(dict.prefix_range("abq").unwrap(), 4..4, "K={interval}");
        assert_eq!(dict.prefix_range("bcd").unwrap(), 7..7, "K={interval}");
    }
}

/// A prefix range must stay contiguous and agree with the keys themselves, whatever the block
/// geometry — that contiguity is the entire reason the dictionary is sorted (§4.3).
#[test]
fn prefix_range_agrees_with_the_keys_it_bounds() {
    let keys: Vec<String> = (0..30).map(|i| format!("p{:02}x", i)).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    for interval in [1, 3, 4, 16] {
        let dict = open(&refs, interval);
        for probe in ["", "p", "p0", "p1", "p2", "p07", "p07x", "p3", "q"] {
            let range = dict.prefix_range(probe).unwrap();
            let expected: Vec<u32> = keys
                .iter()
                .enumerate()
                .filter(|(_, k)| k.starts_with(probe))
                .map(|(i, _)| i as u32)
                .collect();
            let got: Vec<u32> = range.clone().collect();
            assert_eq!(got, expected, "prefix {probe:?} at K={interval}");
        }
    }
}

#[test]
fn key_of_at_the_first_and_last_ordinal_of_every_block() {
    let keys: Vec<String> = (0..37).map(|i| format!("key{:03}", i)).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let interval = 4;
    let dict = open(&refs, interval);
    assert_eq!(dict.block_count(), 10, "37 keys in blocks of 4");
    let mut scratch = Vec::new();
    for block in 0..dict.block_count() {
        let first = block * interval;
        let last = ((block + 1) * interval - 1).min(dict.len() - 1);
        assert_eq!(
            dict.key_of(first, &mut scratch).unwrap(),
            keys[first as usize]
        );
        assert_eq!(
            dict.key_of(last, &mut scratch).unwrap(),
            keys[last as usize]
        );
    }
    // Every ordinal, in a deliberately unhelpful order, through one reused scratch buffer.
    for i in (0..dict.len()).rev() {
        assert_eq!(dict.key_of(i, &mut scratch).unwrap(), keys[i as usize]);
    }
    assert!(matches!(
        dict.key_of(37, &mut scratch),
        Err(DictError::OrdinalOutOfRange {
            ordinal: 37,
            len: 37
        })
    ));
}

#[test]
fn the_walk_visits_every_key_exactly_once_in_order() {
    let keys: Vec<String> = (0..100).map(|i| format!("w{:03}", i)).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    for interval in [1, 5, 16, 128] {
        let dict = open(&refs, interval);
        let mut seen = Vec::new();
        dict.walk(|ordinal, key| seen.push((ordinal, key.to_string())))
            .unwrap();
        assert_eq!(seen.len(), 100, "K={interval}");
        for (i, (ordinal, key)) in seen.iter().enumerate() {
            assert_eq!(*ordinal, i as u32);
            assert_eq!(key, &keys[i]);
        }
    }
}

/// Every operation cross-checked against a plain sorted `Vec`, over pseudo-random key sets at
/// pseudo-random restart intervals.
///
/// This exists because the hand-written cases above missed a real defect: the order check inside a
/// block is local — it compares one byte at the shared prefix's end — but a restart's `shared` is
/// forced to 0, so the same check at a block boundary compared byte 0 of two keys that legitimately
/// shared a prefix, and every dictionary whose block boundary fell inside a run of related keys
/// refused itself. A fixed key list only finds that when a boundary happens to land there. Small
/// alphabets are used deliberately: they make shared prefixes, and therefore boundaries inside
/// them, common.
#[test]
fn every_operation_agrees_with_a_sorted_vec() {
    // xorshift64*, so the corpus is reproducible without a dependency.
    let mut state = 0x2026_0813_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for round in 0..200 {
        let alphabet: &[u8] = match round % 3 {
            0 => b"ab",           // long shared prefixes, many boundaries inside them
            1 => b"abcde",        //
            _ => b"abcdefghijkl", // sparser sharing
        };
        let count = 1 + (next() % 60) as usize;
        let mut keys: Vec<String> = (0..count)
            .map(|_| {
                let len = 1 + (next() % 8) as usize;
                (0..len)
                    .map(|_| alphabet[(next() % alphabet.len() as u64) as usize] as char)
                    .collect()
            })
            .collect();
        keys.sort();
        keys.dedup();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let interval = 1 + (next() % 8) as u32;
        let dict = open(&refs, interval);
        let label = format!("round {round}, {} keys at K={interval}", keys.len());

        dict.self_check().unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(keys_of(&dict), keys, "{label}");
        assert_eq!(dict.len(), keys.len() as u32, "{label}");

        let mut scratch = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(
                dict.resolve(key).unwrap(),
                Some(i as u32),
                "{label}: {key:?}"
            );
            assert_eq!(dict.key_of(i as u32, &mut scratch).unwrap(), key, "{label}");
        }

        // Probe with every string over the alphabet up to length 3, present or not, as both a
        // needle and a prefix.
        let mut probes = vec![String::new()];
        for _ in 0..3 {
            let mut grown = Vec::new();
            for base in &probes {
                for &c in alphabet {
                    grown.push(format!("{base}{}", c as char));
                }
            }
            probes.extend(grown);
        }
        for probe in &probes {
            let expected = keys.binary_search(probe).ok().map(|i| i as u32);
            assert_eq!(dict.resolve(probe).unwrap(), expected, "{label}: {probe:?}");

            let range = dict.prefix_range(probe).unwrap();
            let want: Vec<u32> = keys
                .iter()
                .enumerate()
                .filter(|(_, k)| k.starts_with(probe.as_str()))
                .map(|(i, _)| i as u32)
                .collect();
            assert_eq!(
                range.collect::<Vec<u32>>(),
                want,
                "{label}: prefix {probe:?}"
            );
        }
    }
}

// -------------------------------------------------------------------------------------------
// The writer refuses rather than repairs
// -------------------------------------------------------------------------------------------

#[test]
fn the_empty_string_is_not_a_key() {
    let mut writer = SortedDictWriter::new(Vec::new()).unwrap();
    assert!(malformed(writer.push("")).contains("empty string"));
    // And it is refused first, not merely out of order.
    writer.push("a").unwrap();
    assert!(malformed(writer.push("")).contains("empty string"));
}

#[test]
fn duplicate_and_out_of_order_keys_refuse_at_build() {
    let mut writer = SortedDictWriter::new(Vec::new()).unwrap();
    writer.push("apple").unwrap();
    writer.push("banana").unwrap();
    // A repeat: de-duplicating silently would shift every ordinal after it under a caller that is
    // building the ordinal column against this sequence.
    assert!(malformed(writer.push("banana")).contains("ascend strictly"));
    // Backwards.
    assert!(malformed(writer.push("apple")).contains("ascend strictly"));
    // A strict prefix of the predecessor sorts below it.
    assert!(malformed(writer.push("banan")).contains("ascend strictly"));
    // The writer is still usable for a key that does ascend.
    writer.push("cherry").unwrap();
    assert_eq!(writer.finish().unwrap().keys, 3);
}

#[test]
fn a_zero_restart_interval_refuses() {
    match SortedDictWriter::with_restart_interval(Vec::new(), 0) {
        Err(DictError::Malformed(detail)) => assert!(detail.contains("restart interval of 0")),
        _ => panic!("a restart interval of 0 names no block and must refuse"),
    }
}

// -------------------------------------------------------------------------------------------
// Fail-closed reads
// -------------------------------------------------------------------------------------------

/// Every truncation of a good file refuses. The footer's counts must account for the file exactly,
/// so there is no prefix of a dictionary that is also a dictionary.
#[test]
fn every_truncation_refuses() {
    let good = build(&["alpha", "alpine", "beta", "delta", "gamma"], 2);
    for cut in 0..good.len() {
        let result = SortedDict::from_vec(good[..cut].to_vec());
        assert!(
            result.is_err(),
            "a {cut}-byte prefix of a {}-byte dictionary opened",
            good.len()
        );
    }
    // And a file with bytes appended is equally unaccounted for.
    let mut longer = good.clone();
    longer.push(0);
    assert!(SortedDict::from_vec(longer).is_err());
    SortedDict::from_vec(good).unwrap();
}

#[test]
fn a_corrupted_restart_offset_refuses() {
    let good = build(&["aa", "ab", "ba", "bb", "ca", "cb"], 2);
    let layout = Layout::of(&good);
    assert_eq!(layout.block_count, 3);

    // Past the end of the blocks region.
    let mut far = good.clone();
    far[layout.restarts_at + 8..layout.restarts_at + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(malformed(SortedDict::from_vec(far).unwrap().self_check()).contains("blocks region"));

    // In bounds but wrong — block 1 now starts one byte late, so its first entry reads the tail of
    // block 0's and the block no longer decodes as a restart.
    let mut skewed = good.clone();
    let was = u64::from_le_bytes(
        good[layout.restarts_at + 8..layout.restarts_at + 16]
            .try_into()
            .unwrap(),
    );
    skewed[layout.restarts_at + 8..layout.restarts_at + 16]
        .copy_from_slice(&(was + 1).to_le_bytes());
    let dict = SortedDict::from_vec(skewed).unwrap();
    assert!(dict.self_check().is_err(), "a skewed restart must refuse");

    // Backwards: block 1 would end before it starts.
    let mut backwards = good.clone();
    backwards[layout.restarts_at + 8..layout.restarts_at + 16].copy_from_slice(&0u64.to_le_bytes());
    let dict = SortedDict::from_vec(backwards).unwrap();
    assert!(dict.self_check().is_err());

    // The first offset and the sentinel are checked at open, not deferred.
    let mut sentinel = good.clone();
    let at = layout.restarts_at + layout.block_count * 8;
    sentinel[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
    assert!(malformed(SortedDict::from_vec(sentinel)).contains("restart table"));
}

/// Keys out of order *in the file* — the case the writer cannot prevent, because the damage
/// happens after it. A dictionary that answered from an unordered block would resolve a needle to
/// the wrong ordinal.
#[test]
fn keys_out_of_order_in_the_file_refuse() {
    // One block: [0,2,'a','a'] [1,1,'b'] [1,1,'c'].
    let good = build(&["aa", "ab", "ac"], 16);
    let layout = Layout::of(&good);
    let blocks = &good[layout.blocks_at..layout.blocks_at + layout.blocks_len];
    let at = layout.blocks_at
        + blocks
            .iter()
            .rposition(|&b| b == b'c')
            .expect("the third key's suffix");

    // 'c' -> 'a' makes the third key "aa", a repeat of the first and below the second.
    let mut repeated = good.clone();
    repeated[at] = b'a';
    let dict = SortedDict::from_vec(repeated).unwrap();
    assert!(malformed(dict.self_check()).contains("does not follow its predecessor"));

    // A zero-length suffix is the same fault stated differently: the key repeats its predecessor.
    let mut empty_suffix = good.clone();
    empty_suffix[at - 1] = 0;
    let dict = SortedDict::from_vec(empty_suffix).unwrap();
    // The block no longer tiles its extent, or the suffix is empty — either way it refuses.
    assert!(dict.self_check().is_err());
}

/// A block whose first entry claims a shared prefix cannot be reached by the binary search, which
/// reads that entry as a whole key.
#[test]
fn a_block_that_does_not_restart_refuses() {
    let good = build(&["aa", "ab", "ba", "bb"], 2);
    let layout = Layout::of(&good);
    // Block 1 starts at restarts[1]; its first byte is the shared length, written as 0.
    let block1 = u64::from_le_bytes(
        good[layout.restarts_at + 8..layout.restarts_at + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    let mut doctored = good.clone();
    assert_eq!(
        doctored[layout.blocks_at + block1],
        0,
        "a restart shares nothing"
    );
    doctored[layout.blocks_at + block1] = 1;
    let dict = SortedDict::from_vec(doctored).unwrap();
    assert!(malformed(dict.self_check()).contains("restart shares nothing"));
}

#[test]
fn a_doctored_footer_refuses() {
    let good = build(&["aa", "ab", "ba", "bb", "ca"], 2);
    let layout = Layout::of(&good);

    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert!(malformed(SortedDict::from_vec(bad_magic)).contains("TSDC"));

    let mut bad_trailer = good.clone();
    let last = good.len() - 1;
    bad_trailer[last] = b'X';
    assert!(malformed(SortedDict::from_vec(bad_trailer)).contains("TSDC"));

    let mut bad_version = good.clone();
    bad_version[layout.footer_at..layout.footer_at + 4]
        .copy_from_slice(&(DICT_FORMAT_VERSION + 1).to_le_bytes());
    assert!(malformed(SortedDict::from_vec(bad_version)).contains("format version"));

    // A key count the block count no longer implies.
    let mut bad_keys = good.clone();
    bad_keys[layout.footer_at + 8..layout.footer_at + 12].copy_from_slice(&99u32.to_le_bytes());
    assert!(malformed(SortedDict::from_vec(bad_keys)).contains("blocks"));

    // A restart interval the block count no longer implies.
    let mut bad_interval = good.clone();
    bad_interval[layout.footer_at + 4..layout.footer_at + 8].copy_from_slice(&7u32.to_le_bytes());
    assert!(SortedDict::from_vec(bad_interval).is_err());

    let mut zero_interval = good.clone();
    zero_interval[layout.footer_at + 4..layout.footer_at + 8].copy_from_slice(&0u32.to_le_bytes());
    assert!(malformed(SortedDict::from_vec(zero_interval)).contains("restart interval of 0"));

    // A blocks region larger than the file it sits in — bounded before it is used in arithmetic.
    let mut huge = good.clone();
    huge[layout.footer_at + 16..layout.footer_at + 24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(SortedDict::from_vec(huge).is_err());
}

/// A key that is not UTF-8 refuses at decode rather than being handed on. The writer cannot
/// produce one — it takes `&str` — so this is damage after the fact.
#[test]
fn a_key_that_is_not_utf8_refuses() {
    let good = build(&["aa", "ab"], 16);
    let layout = Layout::of(&good);
    let blocks = &good[layout.blocks_at..layout.blocks_at + layout.blocks_len];
    let at = layout.blocks_at + blocks.iter().rposition(|&b| b == b'b').unwrap();
    let mut doctored = good.clone();
    doctored[at] = 0xff; // never a byte of valid UTF-8
    let dict = SortedDict::from_vec(doctored).unwrap();
    assert!(malformed(dict.self_check()).contains("UTF-8"));
}

// -------------------------------------------------------------------------------------------
// Through the file system
// -------------------------------------------------------------------------------------------

#[test]
fn the_same_answers_read_and_mapped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DICT_FILE);
    let keys: Vec<String> = (0..50).map(|i| format!("2401.{:05}", i * 3)).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let stats = write_sorted_dict(&path, refs.iter().copied()).unwrap();
    assert_eq!(stats.keys, 50);
    assert_eq!(stats.bytes, std::fs::metadata(&path).unwrap().len());

    for access in [Access::Read, Access::Mapped, Access::MappedSequential] {
        let dict = SortedDict::open(&path, access).unwrap();
        dict.self_check().unwrap();
        assert_eq!(keys_of(&dict), keys, "{access:?}");
        assert_eq!(dict.resolve("2401.00042").unwrap(), Some(14), "{access:?}");
        assert_eq!(dict.resolve("2401.00043").unwrap(), None, "{access:?}");
        assert_eq!(dict.prefix_range("2401.0000").unwrap(), 0..4, "{access:?}");
    }
    // `open_dir` finds the canonical name.
    let dict = SortedDict::open_dir(dir.path(), Access::Read).unwrap();
    assert_eq!(dict.len(), 50);
}

#[test]
fn a_short_file_refuses_before_it_is_mapped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DICT_FILE);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(b"TS")
        .unwrap();
    for access in [Access::Read, Access::Mapped] {
        assert!(malformed(SortedDict::open(&path, access)).contains("minimum"));
    }
    // A zero-length file too — the length check runs before `Mmap::map`, which rejects an empty
    // mapping outright.
    std::fs::write(&path, b"").unwrap();
    assert!(SortedDict::open(&path, Access::Mapped).is_err());
}

/// [`KeyMatcher`] is a substitution for `str::contains` inside the `contains` routes, so what it
/// owes is *the same answer*, not a similar one — the two are compared over every pair of a key
/// set and a needle set chosen for the cases a byte-level searcher gets wrong: a needle longer
/// than the key, a needle that is the whole key, overlapping repeats, a multi-byte character split
/// down the middle of the needle, and the empty needle every key contains.
#[test]
fn a_key_matcher_agrees_with_str_contains() {
    let keys = [
        "", "a", "aa", "aaa", "abcabcabc", "banana", "café", "naïve café", "日本語のテキスト",
        "2401.00042", "\u{1f600}emoji", "a\u{0}b",
    ];
    let needles = [
        "", "a", "aa", "aaa", "abc", "cab", "ana", "anana", "banana", "bananas", "é", " café",
        "日本", "語の", "\u{1f600}", "\u{0}", "zzz", "2401.00042", "2401.000420",
    ];
    for needle in needles {
        let matcher = KeyMatcher::new(needle);
        for key in keys {
            assert_eq!(
                matcher.matches(key),
                key.contains(needle),
                "{key:?} contains {needle:?}"
            );
        }
    }
}

/// The same agreement through a whole dictionary walk, which is how the broad `contains` route
/// uses it: the matcher is built once and the ordinals it selects must be the ones the per-key
/// `str::contains` selects.
#[test]
fn a_matcher_selects_the_same_ordinals_as_a_per_key_search() {
    let keys: Vec<String> = (0..200).map(|i| format!("dept-{i:04}-of-{}", i % 7)).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let dict = open(&refs, 16);
    for needle in ["", "dept", "-of-3", "0042", "0199", "zzz", "dept-0000-of-0"] {
        let matcher = KeyMatcher::new(needle);
        let mut hoisted = Vec::new();
        dict.walk(|o, key| {
            if matcher.matches(key) {
                hoisted.push(o);
            }
        })
        .unwrap();
        let mut per_key = Vec::new();
        dict.walk(|o, key| {
            if key.contains(needle) {
                per_key.push(o);
            }
        })
        .unwrap();
        assert_eq!(hoisted, per_key, "needle {needle:?}");
    }
}

// -------------------------------------------------------------------------------------------
// The block-restricted walk

/// [`SortedDict::walk_ordinals`] is [`SortedDict::key_of`] amortised over a block, so what it owes
/// is *the same keys in the same order*. Compared against probing each ordinal separately, over
/// subsets chosen for the block-grouping cases: every ordinal, alternating ones, a stride coprime
/// with every interval tried, the boundaries of the first and last blocks, three inside one block,
/// a single key, and none at all.
#[test]
fn walk_ordinals_agrees_with_probing_each_key() {
    let keys: Vec<String> = (0..200).map(|i| format!("key-{i:04}")).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let subsets: [Vec<u32>; 8] = [
        vec![],
        vec![0],
        vec![199],
        (0..200).collect(),
        (0..200).step_by(2).collect(),
        (0..200).step_by(17).collect(),
        vec![0, 1, 15, 16, 17, 31, 32, 199],
        vec![95, 96, 97],
    ];
    for interval in [1, 2, 16, 64, 256] {
        let dict = open(&refs, interval);
        for wanted in &subsets {
            let mut seen: Vec<(u32, String)> = Vec::new();
            dict.walk_ordinals(wanted, |ordinal, key| seen.push((ordinal, key.to_string())))
                .unwrap();
            let mut scratch = Vec::new();
            let probed: Vec<(u32, String)> = wanted
                .iter()
                .map(|&o| (o, dict.key_of(o, &mut scratch).unwrap().to_string()))
                .collect();
            assert_eq!(seen, probed, "interval {interval}, {} wanted", wanted.len());
        }
    }
}

/// A list that does not ascend strictly is refused rather than silently under-answered: the block
/// grouping and the per-block cursor both assume it, and a duplicate or a step backwards would
/// skip keys the caller asked for.
#[test]
fn walk_ordinals_refuses_a_list_that_does_not_ascend() {
    let dict = open(&["a", "b", "c", "d"], 2);
    for bad in [vec![1, 1], vec![2, 1], vec![0, 3, 2], vec![3, 0]] {
        let detail = malformed(dict.walk_ordinals(&bad, |_, _| {}));
        assert!(detail.contains("ascend strictly"), "{bad:?}: {detail}");
    }
}

/// An ordinal past the end takes the variant that names a *pair* that disagrees — an ordinal
/// column read against the wrong layer's dictionary — and not the one that means this file is
/// damaged. Both the first position and a later one, since the bound is checked per block group.
#[test]
fn walk_ordinals_refuses_an_ordinal_past_the_end() {
    let dict = open(&["a", "b", "c", "d"], 2);
    for wanted in [vec![4], vec![0, 9], vec![0, 1, 2, 3, 4]] {
        assert!(
            matches!(
                dict.walk_ordinals(&wanted, |_, _| {}),
                Err(DictError::OrdinalOutOfRange { len: 4, .. })
            ),
            "{wanted:?}"
        );
    }
}

/// A damaged block refuses when the walk opens it — **and only then**, which is the honest reading
/// of a restricted walk and the reason [`SortedDict::self_check`] remains the exhaustive pass. It
/// has exactly `key_of`'s reach: a caller asking for keys in intact blocks gets them, and a caller
/// whose ordinals land in the damaged one is refused rather than answered.
#[test]
fn walk_ordinals_refuses_a_damaged_block_and_no_other() {
    let good = build(&["aa", "ab", "ba", "bb"], 2);
    let layout = Layout::of(&good);
    let block1 = u64::from_le_bytes(
        good[layout.restarts_at + 8..layout.restarts_at + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    let mut doctored = good.clone();
    assert_eq!(doctored[layout.blocks_at + block1], 0, "a restart shares nothing");
    doctored[layout.blocks_at + block1] = 1;
    let dict = SortedDict::from_vec(doctored).unwrap();

    // Block 0 is intact, so the keys in it are still answerable.
    let mut seen = Vec::new();
    dict.walk_ordinals(&[0, 1], |o, key| seen.push((o, key.to_string())))
        .unwrap();
    assert_eq!(seen, vec![(0, "aa".to_string()), (1, "ab".to_string())]);

    // Any list reaching block 1 refuses, whether or not it also names an intact block.
    for wanted in [vec![2], vec![3], vec![0, 2]] {
        assert!(
            malformed(dict.walk_ordinals(&wanted, |_, _| {})).contains("restart shares nothing"),
            "{wanted:?}"
        );
    }
    assert!(dict.self_check().is_err(), "the exhaustive pass refuses it");
}
