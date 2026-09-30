//! The streaming keyed-postings writer against the buffered one. Byte-identity is the assertion:
//! the two writers must agree on bytes rather than on readback.

use tessera_authz::{write_delta_tier_at, DeltaTier, KeyedPostingsSpool, PostingRef};

const SMALL: u32 = 32;

fn entities(posting: PostingRef<'_>) -> Vec<u32> {
    match posting {
        PostingRef::Roaring(view) => view.iter().collect(),
        PostingRef::Array(bytes) => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect(),
    }
}

/// Entries spanning both encodings: below the threshold (tag 0) and far above it (tag 1).
fn entries() -> Vec<(u32, Vec<u32>)> {
    vec![
        (0, vec![0]),
        (7, (0..5).collect()),
        (99, (0..10_000).map(|e| e * 3).collect()),
        (100_000, vec![7, 9, 11]),
        (4_000_000_000, (0..40).collect()),
    ]
}

#[test]
fn a_spooled_file_is_the_buffered_writer_s_bytes_however_it_is_banded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = entries();
    let buffered = dir.path().join("buffered.arrow");
    write_delta_tier_at(&buffered, &entries, SMALL).expect("buffered");
    let expected = std::fs::read(&buffered).expect("read");

    // Every partition of the key space into consecutive bands, appended band by band: the writer
    // must not be able to tell where a band ended.
    for cut_a in 0..=entries.len() {
        for cut_b in cut_a..=entries.len() {
            let path = dir.path().join("spooled.arrow");
            let mut spool =
                KeyedPostingsSpool::create(&dir.path().join("spool"), SMALL).expect("create");
            for band in [&entries[..cut_a], &entries[cut_a..cut_b], &entries[cut_b..]] {
                for (key, e) in band {
                    spool.append(*key, e).expect("append");
                }
            }
            assert_eq!(spool.len(), entries.len());
            spool.finish(&path).expect("finish");
            assert_eq!(
                std::fs::read(&path).expect("read"),
                expected,
                "bands cut at {cut_a}, {cut_b}"
            );
            assert!(
                !dir.path().join("spool").exists(),
                "the spool outlived the file"
            );
        }
    }
}

#[test]
fn an_empty_keyed_file_is_the_buffered_writer_s_bytes_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let buffered = dir.path().join("buffered.arrow");
    write_delta_tier_at(&buffered, &[], SMALL).expect("buffered");
    let spool = KeyedPostingsSpool::create(&dir.path().join("spool"), SMALL).expect("create");
    assert!(spool.is_empty());
    let path = dir.path().join("spooled.arrow");
    spool.finish(&path).expect("finish");
    assert_eq!(
        std::fs::read(&path).expect("read"),
        std::fs::read(&buffered).expect("read")
    );
}

#[test]
fn a_spooled_file_reads_back_through_the_ordinary_reader() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("spooled.arrow");
    let mut spool = KeyedPostingsSpool::create(&dir.path().join("spool"), SMALL).expect("create");
    for (key, e) in entries() {
        spool.append(key, &e).expect("append");
    }
    spool.finish(&path).expect("finish");

    let tier = DeltaTier::open(&path).expect("open");
    for (key, e) in entries() {
        let posting = tier.posting_at(key).expect("read").expect("carried");
        assert_eq!(entities(posting), e, "key {key}");
    }
    assert!(tier.posting_at(8).expect("read").is_none());
}

#[test]
fn a_key_that_does_not_ascend_is_refused_across_a_band_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spool = KeyedPostingsSpool::create(&dir.path().join("spool"), SMALL).expect("create");
    spool.append(4, &[1]).expect("append");
    spool.append(9, &[2]).expect("append");
    let err = spool
        .append(9, &[3])
        .expect_err("a repeated key is refused");
    assert!(err.to_string().contains("ascending"), "{err}");
    let err = spool.append(5, &[3]).expect_err("a lower key is refused");
    assert!(err.to_string().contains("ascending"), "{err}");
}

#[test]
fn an_unsorted_entity_list_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spool = KeyedPostingsSpool::create(&dir.path().join("spool"), SMALL).expect("create");
    spool
        .append(1, &[5, 4])
        .expect_err("the encoder checks sortedness");
}
