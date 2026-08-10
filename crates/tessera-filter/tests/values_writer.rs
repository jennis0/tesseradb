//! The streaming value-column writer against the whole-column one.
//!
//! **Byte-identity is the assertion, not equivalence.** The fold's claim is that a folded column
//! and a freshly built one over the same live entities are the same bytes (`filter-index.md`
//! §6.2), and the two writers are what that claim rests on — so the test compares files, not
//! readbacks.

use std::path::Path;

use croaring::Bitmap;
use tessera_filter::{write_value_column, Codes, ColumnKind, ValueColumn, ValueColumnWriter};

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("the writer wrote a file")
}

/// The one-shot file and the streamed one, for the same values pushed in chunks of `chunk`.
fn both(
    dir: &Path,
    whole: &Codes,
    chunks: impl IntoIterator<Item = Codes>,
    presence: Option<&Bitmap>,
) -> (Vec<u8>, Vec<u8>) {
    let one = dir.join("one.arrow");
    let one_presence = dir.join("one.roaring");
    write_value_column(&one, &one_presence, whole, presence).expect("one-shot");

    let streamed = dir.join("streamed.arrow");
    let streamed_presence = dir.join("streamed.roaring");
    let mut w = ValueColumnWriter::create(&streamed, &streamed_presence, ColumnKind::of(whole))
        .expect("create");
    for chunk in chunks {
        w.push(&chunk).expect("push");
    }
    w.finish(presence).expect("finish");

    // The spools are transients, not artefacts: a caller that has finished a column must be left
    // with the two files the format names and nothing else.
    for stray in std::fs::read_dir(dir).expect("dir") {
        let name = stray.expect("entry").file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.contains("spool") && !name.contains("offsets"),
            "the writer left {name} behind"
        );
    }
    if presence.is_some() {
        assert_eq!(read(&one_presence), read(&streamed_presence));
    }
    (read(&one), read(&streamed))
}

#[test]
fn fixed_width_columns_stream_byte_identically_at_every_chunking() {
    let dir = tempfile::tempdir().expect("tempdir");
    for chunk in [1usize, 2, 7, 999, 5_000] {
        let values: Vec<u32> = (0..5_000u32)
            .map(|e| e.wrapping_mul(2_654_435_761))
            .collect();
        let whole = Codes::U32(values.clone().into());
        let chunks = values
            .chunks(chunk)
            .map(|c| Codes::U32(c.to_vec().into()))
            .collect::<Vec<_>>();
        let (one, streamed) = both(dir.path(), &whole, chunks, None);
        assert_eq!(one, streamed, "chunk size {chunk}");
    }
}

#[test]
fn every_fixed_width_family_streams_byte_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 300usize;
    let wholes = vec![
        Codes::U8((0..n).map(|i| i as u8).collect::<Vec<_>>().into()),
        Codes::U16((0..n).map(|i| i as u16).collect::<Vec<_>>().into()),
        Codes::U32((0..n).map(|i| i as u32 * 7).collect::<Vec<_>>().into()),
        Codes::U64((0..n).map(|i| (i as u64) << 40).collect::<Vec<_>>().into()),
        Codes::I8((0..n).map(|i| i as i8).collect::<Vec<_>>().into()),
        Codes::I16((0..n).map(|i| -(i as i16)).collect::<Vec<_>>().into()),
        Codes::I32((0..n).map(|i| -(i as i32) * 9).collect::<Vec<_>>().into()),
        Codes::I64((0..n).map(|i| -(i as i64) << 33).collect::<Vec<_>>().into()),
        Codes::F32((0..n).map(|i| i as f32 / 3.0).collect::<Vec<_>>().into()),
        Codes::F64((0..n).map(|i| i as f64 / 7.0).collect::<Vec<_>>().into()),
    ];
    for whole in &wholes {
        // Halved: enough to exercise a boundary in the middle of the column.
        let chunks = split(whole, n / 2);
        let (one, streamed) = both(dir.path(), whole, chunks, None);
        assert_eq!(one, streamed, "{:?}", ColumnKind::of(whole));
    }
}

/// Split a column into two chunks at `at`, in the family it already is.
fn split(whole: &Codes, at: usize) -> Vec<Codes> {
    macro_rules! halves {
        ($v:expr, $ctor:expr) => {{
            let v = $v;
            vec![
                $ctor(v[..at].to_vec().into()),
                $ctor(v[at..].to_vec().into()),
            ]
        }};
    }
    match whole {
        Codes::U8(v) => halves!(v, Codes::U8),
        Codes::U16(v) => halves!(v, Codes::U16),
        Codes::U32(v) => halves!(v, Codes::U32),
        Codes::U64(v) => halves!(v, Codes::U64),
        Codes::I8(v) => halves!(v, Codes::I8),
        Codes::I16(v) => halves!(v, Codes::I16),
        Codes::I32(v) => halves!(v, Codes::I32),
        Codes::I64(v) => halves!(v, Codes::I64),
        Codes::F32(v) => halves!(v, Codes::F32),
        Codes::F64(v) => halves!(v, Codes::F64),
        Codes::Text { .. } => unreachable!("text splits by value, not by slice"),
    }
}

#[test]
fn a_text_column_streams_byte_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The empty string is a value a corpus may legitimately hold, and a multi-byte character is
    // what makes the offsets more than a length count.
    let values: Vec<String> = (0..1_000)
        .map(|i| match i % 4 {
            0 => String::new(),
            1 => format!("value-{i}"),
            2 => "naïve—dash".to_string(),
            _ => "x".repeat(i % 37),
        })
        .collect();
    for chunk in [1usize, 3, 128, 1_000] {
        let whole = Codes::text(values.clone());
        let chunks: Vec<Codes> = values
            .chunks(chunk)
            .map(|c| Codes::text(c.to_vec()))
            .collect();
        let (one, streamed) = both(dir.path(), &whole, chunks, None);
        assert_eq!(one, streamed, "chunk size {chunk}");
    }
}

#[test]
fn an_empty_column_streams_byte_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (one, streamed) = both(dir.path(), &Codes::U32(Vec::<u32>::new().into()), [], None);
    assert_eq!(one, streamed);
    let (one, streamed) = both(dir.path(), &Codes::text(Vec::<String>::new()), [], None);
    assert_eq!(one, streamed);
}

#[test]
fn a_partial_column_streams_its_presence_bitmap_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut presence = Bitmap::new();
    let values: Vec<u16> = (0..500u16).collect();
    for (k, _) in values.iter().enumerate() {
        presence.add(k as u32 * 3 + 11);
    }
    let whole = Codes::U16(values.clone().into());
    let chunks: Vec<Codes> = values
        .chunks(64)
        .map(|c| Codes::U16(c.to_vec().into()))
        .collect();
    let (one, streamed) = both(dir.path(), &whole, chunks, Some(&presence));
    assert_eq!(one, streamed);

    // And the column reads back rank-addressed, which is what the presence file is for.
    let opened = ValueColumn::open(
        &dir.path().join("streamed.arrow"),
        Some(&dir.path().join("streamed.roaring")),
        tessera_filter::Access::Read,
    )
    .expect("open");
    assert_eq!(opened.value_of(11).map(|v| v.raw()), Some(0));
    assert_eq!(opened.value_of(14).map(|v| v.raw()), Some(1));
    assert_eq!(opened.value_of(12), None);
}

#[test]
fn a_chunk_of_the_wrong_family_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut w = ValueColumnWriter::create(
        &dir.path().join("v.arrow"),
        &dir.path().join("p.roaring"),
        ColumnKind::U32,
    )
    .expect("create");
    w.push(&Codes::U32(vec![1u32, 2].into())).expect("push");
    // A `u16` chunk would be spooled as two bytes a value and read back as four — every value
    // after it addressed wrongly, with no error anywhere.
    let err = w.push(&Codes::U16(vec![3u16].into())).expect_err("refused");
    assert!(err.to_string().contains("declared"), "{err}");
}

#[test]
fn a_presence_bitmap_that_does_not_match_the_value_count_is_refused_by_both_writers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut presence = Bitmap::new();
    presence.add_range(0u32..5);

    let err = write_value_column(
        &dir.path().join("v.arrow"),
        &dir.path().join("p.roaring"),
        &Codes::U32(vec![1u32, 2, 3].into()),
        Some(&presence),
    )
    .expect_err("refused");
    assert!(err.to_string().contains("presence has 5"), "{err}");

    let mut w = ValueColumnWriter::create(
        &dir.path().join("v2.arrow"),
        &dir.path().join("p2.roaring"),
        ColumnKind::U32,
    )
    .expect("create");
    w.push(&Codes::U32(vec![1u32, 2, 3].into())).expect("push");
    let err = w.finish(Some(&presence)).expect_err("refused");
    assert!(err.to_string().contains("presence has 5"), "{err}");
}

#[test]
fn an_abandoned_writer_leaves_no_spool_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let mut w = ValueColumnWriter::create(
            &dir.path().join("v.arrow"),
            &dir.path().join("p.roaring"),
            ColumnKind::Text,
        )
        .expect("create");
        w.push(&Codes::text(vec!["a".to_string(); 100]))
            .expect("push");
    }
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
