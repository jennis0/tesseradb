//! The oracle: the campaign's expected answers, read from the **source parquet**.
//!
//! Every correctness assertion in this campaign compares the index against values loaded here —
//! the same file the build was given, decoded independently — rather than against another route
//! through the index. Comparing the artefact with itself is the failure mode that makes a
//! consistent-but-wrong writer look correct.
//!
//! Values are held in *source id* space (the points file's `entity_id`), because that is the space
//! the file is written in. Entity ids are assigned by the authorisation signature sort and are not
//! source ids, so every lookup goes through the `entity → source` map the campaign maintains: the
//! build's half comes from the external-id extent, the flushed half from what `accept_ingest`
//! returned.

use std::collections::BTreeMap;
use std::path::Path;

use arrow::array::{Array, Int64Array, StringArray, UInt32Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// Every declared column's values, in source-id order.
///
/// Categories are held as their pinned codes, so the comparison is against the code the artefact
/// stores rather than against a key resolved through the same manifest the reader uses. `0` is the
/// reserved absent sentinel for a category (per-point-attributes §3.6); `first_author` uses an
/// empty slice in the offsets to mean absent.
pub struct Oracle {
    pub archive: Vec<u8>,
    pub primary: Vec<u16>,
    pub secondary: Vec<u16>,
    /// `first_author`, concatenated; `author_at(i)` slices it. `None` where the source was null.
    author_bytes: Vec<u8>,
    author_offsets: Vec<u32>,
    author_present: Vec<bool>,
    pub submitted_at: Vec<i64>,
    /// Code → key, per category column. The ingest path needs the *key* (a category arrives at
    /// ingest as its key, never a code — contracts §2.4), and holding one `String` per entity for
    /// three columns costs several gigabytes at 25M for data that is a few hundred distinct values.
    pub archive_keys: Vec<String>,
    pub primary_keys: Vec<String>,
    pub secondary_keys: Vec<String>,
}

impl Oracle {
    pub fn author_at(&self, source: usize) -> Option<&str> {
        if !self.author_present[source] {
            return None;
        }
        let lo = self.author_offsets[source] as usize;
        let hi = self.author_offsets[source + 1] as usize;
        Some(std::str::from_utf8(&self.author_bytes[lo..hi]).expect("utf8"))
    }

    pub fn archive_key(&self, source: usize) -> &str {
        &self.archive_keys[self.archive[source] as usize]
    }
    pub fn primary_key(&self, source: usize) -> &str {
        &self.primary_keys[self.primary[source] as usize]
    }
    pub fn secondary_key(&self, source: usize) -> &str {
        &self.secondary_keys[self.secondary[source] as usize]
    }
}

/// Read `points.parquet` into source-id order, resolving category keys through the vocabulary
/// files' pinned codes.
///
/// The file is not sorted by `entity_id` (it is Morton-ordered), so every column is scattered into
/// place by id rather than appended — which is also why `total` must be known up front.
pub fn load(
    points: &Path,
    total: usize,
    archive_codes: &BTreeMap<String, u32>,
    primary_codes: &BTreeMap<String, u32>,
    secondary_codes: &BTreeMap<String, u32>,
) -> Oracle {
    let file = std::fs::File::open(points).expect("points file opens");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("parquet");
    let reader = builder.build().expect("reader");

    let mut archive = vec![0u8; total];
    let mut primary = vec![0u16; total];
    let mut secondary = vec![0u16; total];
    let mut submitted_at = vec![0i64; total];
    let mut author: Vec<Option<String>> = vec![None; total];
    let reverse = |codes: &BTreeMap<String, u32>| -> Vec<String> {
        let max = codes.values().copied().max().unwrap_or(0) as usize;
        let mut out = vec![String::new(); max + 1];
        for (k, c) in codes {
            out[*c as usize] = k.clone();
        }
        out
    };

    let mut seen = 0usize;
    for batch in reader {
        let batch = batch.expect("batch");
        let idx = |name: &str| batch.schema().index_of(name).expect(name);
        let ids = batch
            .column(idx("entity_id"))
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("entity_id u32");
        let arch = batch
            .column(idx("archive"))
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("archive utf8");
        let prim = batch
            .column(idx("primary_category"))
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("primary_category utf8");
        let sec = batch
            .column(idx("secondary_category"))
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("secondary_category utf8");
        let auth = batch
            .column(idx("first_author"))
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("first_author utf8");
        let ts = batch
            .column(idx("submitted_at"))
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("submitted_at i64");

        for i in 0..batch.num_rows() {
            let e = ids.value(i) as usize;
            if e >= total {
                continue;
            }
            seen += 1;
            let a = arch.value(i);
            archive[e] = *archive_codes.get(a).unwrap_or(&0) as u8;
            let p = prim.value(i);
            primary[e] = *primary_codes.get(p).unwrap_or(&0) as u16;
            if sec.is_valid(i) && !sec.value(i).is_empty() {
                let s = sec.value(i);
                secondary[e] = *secondary_codes.get(s).unwrap_or(&0) as u16;
            }
            if auth.is_valid(i) && !auth.value(i).is_empty() {
                author[e] = Some(auth.value(i).to_string());
            }
            submitted_at[e] = ts.value(i);
        }
    }
    assert_eq!(seen, total, "the points file did not cover [0, {total})");

    let mut author_bytes = Vec::new();
    let mut author_offsets = Vec::with_capacity(total + 1);
    let mut author_present = Vec::with_capacity(total);
    for a in &author {
        author_offsets.push(author_bytes.len() as u32);
        match a {
            Some(s) => {
                author_bytes.extend_from_slice(s.as_bytes());
                author_present.push(true);
            }
            None => author_present.push(false),
        }
    }
    author_offsets.push(author_bytes.len() as u32);

    Oracle {
        archive,
        primary,
        secondary,
        author_bytes,
        author_offsets,
        author_present,
        submitted_at,
        archive_keys: reverse(archive_codes),
        primary_keys: reverse(primary_codes),
        secondary_keys: reverse(secondary_codes),
    }
}

/// Read a vocabulary parquet (`key`, `code`) into the map the build pinned.
pub fn read_vocabulary(path: &Path) -> BTreeMap<String, u32> {
    let file = std::fs::File::open(path).expect("vocabulary opens");
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .expect("parquet")
        .build()
        .expect("reader");
    let mut out = BTreeMap::new();
    for batch in reader {
        let batch = batch.expect("batch");
        let keys = batch
            .column(batch.schema().index_of("key").unwrap())
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("key utf8");
        let codes = batch
            .column(batch.schema().index_of("code").unwrap())
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("code u32");
        for i in 0..batch.num_rows() {
            out.insert(keys.value(i).to_string(), codes.value(i));
        }
    }
    out
}
