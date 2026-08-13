//! **What does replacing the flat `utf8` column with the keyword family actually cost, per
//! operator, on real string columns?**
//!
//! `records-and-search.md` §4.3 retires `utf8` and names three prices to be paid knowingly: write
//! weight, discarded code, and "the `contains` constant-profile shift". Both formats are in the
//! tree at the moment this runs, and only at this moment: once the flat column is deleted there is
//! no baseline left and every later claim about the swap becomes unfalsifiable. This is the
//! regression fence — the same corpus, the same candidates, the same needles, one variable.
//!
//! # The two sides
//!
//! **Flat** is the shipped `utf8` column: [`ValueColumn`] over [`Codes::Text`], answering `eq`,
//! `prefix`, `in` and `contains` by walking the stored bytes under the candidate.
//!
//! **Keyword** is §4.3's family: a front-coded [`SortedDict`] of the column's distinct values (the
//! shipped writer, the shipped `K = 16`) plus a `u32` ordinal per present entity, which is a
//! [`Codes::U32`] value column. Its routes are the ones §4.3 specifies, assembled here from
//! shipped parts because the family's evaluation half is not built:
//!
//! | operator | keyword route |
//! |---|---|
//! | `eq` | [`SortedDict::resolve`], then the ordinal scan for that ordinal |
//! | `prefix` | [`SortedDict::prefix_range`], then `scan_range` over the ordinal range |
//! | `in` | *k* resolves, then `scan_num_in` over the ordinal set |
//! | `contains`, broad | [`SortedDict::walk`] searching every key, then the ordinal set scan |
//! | `contains`, narrow | per candidate entity: `value_of`, [`SortedDict::key_of`], search the key |
//!
//! **`eq` is measured through both shipped ordinal scans**, because the family has a choice and it
//! is not free. `scan_num_eq` treats the ordinal as a number and reaches `walk_typed` through a
//! one-element `binary_search`; `scan_eq` treats it as a code and reaches the same traversal
//! through a direct compare. §4.3 specifies the operation, not which entry point serves it.
//!
//! The scans are shipped code. The two `contains` routes and the loop that drives them are not —
//! §4.3 marks both unbuilt — so they live here, and what they are made of is stated so a reader
//! can judge whether the constant is the route's or the harness's. Nothing in the engine, the
//! filter crates or the build was changed to run this.
//!
//! **The broad route's ordinal test is reported two ways, and the difference is the point.** The
//! shipped set scan (`scan_num_in`) sorts its needles once and binary-searches per slot, which is
//! §4.3's own "O(log k) per slot" — but broad `contains` can yield tens of thousands of ordinals
//! where `in` yields eight, and log k is then a real term. A dense bitset over the dictionary's
//! ordinals answers the same question in O(1) per slot. That variant is **bench-local, not
//! shipped**, and it is here because building the broad route out of `scan_num_in` alone would
//! import a cost the design never intended.
//!
//! **The narrow route deliberately does not memoise `key_of` by ordinal.** Caching would make the
//! running time a function of how many *distinct* values the candidate happens to cover, which is
//! a statistic about the principal's own data leaking into latency — the thing §4.3's
//! "an unresolved needle still scans" rule exists to prevent one instance of. The uncached loop is
//! both the safe route and the one whose cost is a function of `(candidate, column)`.
//!
//! # Needles, fixed before anything was timed
//!
//! Every needle is picked by **position** in the column's value list, at fractions declared as
//! constants below and not revised after a result was seen. Picking by position draws a value
//! frequency-weighted: a repeat-heavy column yields a common value and a unique one yields a
//! singleton, which is what a caller's own query does. `prefix` and `contains` needles are cut
//! from the value at the midpoint by a fixed rule. A deliberately absent needle is measured beside
//! the hits, because §4.3 requires a miss to cost what a hit costs.
//!
//! # What this does not measure
//!
//! - **Scale.** 2.4M real records is what the corpus has. Every 10⁹ figure derived from these
//!   constants is a multiplication and is marked modelled where it is quoted.
//! - **Paging.** Both columns are read into owned buffers (`Access::Read`) and the dictionary
//!   through `from_vec`, so all three structures are resident and heap-backed. That is one
//!   variable — the format — rather than two, and it is *not* the request path's `Access::Mapped`.
//! - **Concurrency.** Single-threaded throughout, as every campaign this reads against was.
//! - **The write side.** §11 item 7's, not this.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin utf8_retirement_fence -- \
//!     --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json \
//!     [--limit 2400000] [--repeat 5]
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use croaring::Bitmap;
use tessera_filter::{
    write_sorted_dict, write_value_column, Access, Codes, Scalar, SortedDict, ValueColumn,
};
use tessera_types::AttrLocalId;

/// Where the `eq` needles are drawn from, as fractions of the value list. Fixed before the first
/// timing run.
const EQ_POSITIONS: [f64; 5] = [0.125, 0.3, 0.5, 0.7, 0.875];
/// Where the `in` needle set is drawn from — eight values, the size a tick-box filter sends.
const IN_POSITIONS: [f64; 8] = [0.05, 0.17, 0.29, 0.41, 0.53, 0.65, 0.77, 0.89];
/// A needle no arXiv identifier, submitter or DOI carries, for the miss rows.
const ABSENT: &str = "\u{1}tessera-absent\u{1}";
/// Candidate cardinalities for cell 1, as fractions of the corpus.
const CANDIDATE_FRACTION: f64 = 0.25;
/// `|candidate| / |dictionary|` points for the `contains` crossover sweep. §4.3's route rule puts
/// the crossover near 0.15; the sweep straddles it by an order of magnitude either side.
const CROSSOVER_RATIOS: [f64; 11] = [
    0.02, 0.05, 0.10, 0.15, 0.20, 0.30, 0.50, 0.75, 1.00, 2.00, 4.00,
];

/// The three real columns prior campaigns used, and what each is a case of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    /// Near-unique: one distinct value per entity.
    Id,
    /// Repeat-heavy: ~4.5 entities per distinct value.
    Submitter,
    /// Sparse and long: present on roughly a third of entities.
    Doi,
}

impl Column {
    fn name(self) -> &'static str {
        match self {
            Column::Id => "id",
            Column::Submitter => "submitter",
            Column::Doi => "doi",
        }
    }

    /// How many bytes of the midpoint value become the `prefix` needle. A DOI's leading `10.` and
    /// registrant prefix are shared corpus-wide, so three bytes there would select nearly the whole
    /// column and measure a full scan under a different name.
    fn prefix_len(self) -> usize {
        match self {
            Column::Id | Column::Submitter => 3,
            Column::Doi => 8,
        }
    }
}

/// A column in both formats, plus the needles it will be asked for.
struct Both {
    column: Column,
    /// Entities carrying a value, ascending. `None` where every entity carries one.
    presence: Option<Bitmap>,
    /// The shipped `utf8` column.
    flat: ValueColumn,
    /// The keyword family's ordinal column.
    ordinals: ValueColumn,
    /// The keyword family's dictionary.
    dict: SortedDict,
    entities: u32,
    distinct: usize,
    eq_needles: Vec<String>,
    in_needles: Vec<String>,
    prefix_needle: String,
    contains_needle: String,
    /// On-disk bytes: flat values, flat presence, dictionary, ordinal values, ordinal presence.
    bytes: [u64; 5],
}

/// Truncate at or below `n` bytes without splitting a character. `str::floor_char_boundary` is
/// unstable, and submitter names are not all ASCII.
fn clip(s: &str, n: usize) -> &str {
    let mut n = n.min(s.len());
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// The `contains` needle: four bytes cut from a third of the way into the midpoint value. A fixed
/// rule, applied before any timing, so the needle's match rate is whatever the corpus makes it.
fn mid_substring(s: &str) -> String {
    let start = {
        let mut i = s.len() / 3;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let rest = &s[start..];
    clip(rest, 4).to_string()
}

fn build(column: Column, values: &[String], dir: &Path) -> Result<Both, Box<dyn std::error::Error>> {
    let entities = values.len() as u32;

    // Present entities and their values in slot order. An empty string is an absent value, which
    // is what the snapshot means by it and what the flat column and the ordinal column must agree
    // about for the comparison to be of one variable.
    let mut presence = Bitmap::new();
    let mut present: Vec<&str> = Vec::with_capacity(values.len());
    for (entity, v) in values.iter().enumerate() {
        if !v.is_empty() {
            presence.add(entity as u32);
            present.push(v);
        }
    }
    presence.run_optimize();
    let universal = presence.cardinality() == entities as u64;

    // The dictionary: distinct values, sorted, through the shipped writer at the shipped K.
    let mut distinct: Vec<&str> = present.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let dict_path = dir.join(format!("{}-dict.bin", column.name()));
    write_sorted_dict(&dict_path, distinct.iter().copied())?;
    let dict = SortedDict::from_vec(std::fs::read(&dict_path)?)?;

    // One `u32` per present entity, naming its value's position in that dictionary. Resolved by
    // binary search over the same sorted vector the dictionary was written from — setup, never
    // timed, and it cannot disagree with the dictionary because it is the same sequence.
    let ordinals: Vec<u32> = present
        .iter()
        .map(|v| distinct.binary_search(v).expect("a present value is distinct") as u32)
        .collect();

    // Both columns written through the shipped writer and read back through the shipped reader, so
    // the bytes reported are the artefact's and the scans run over what a bundle would hold.
    let flat_values = dir.join(format!("{}-flat-values.arrow", column.name()));
    let flat_presence = dir.join(format!("{}-flat-presence.roaring", column.name()));
    let ord_values = dir.join(format!("{}-ord-values.arrow", column.name()));
    let ord_presence = dir.join(format!("{}-ord-presence.roaring", column.name()));
    let text = Codes::text(present.iter().map(|s| s.to_string()));
    let codes = Codes::U32(ordinals.into());
    let p = (!universal).then_some(&presence);
    write_value_column(&flat_values, &flat_presence, &text, p)?;
    write_value_column(&ord_values, &ord_presence, &codes, p)?;
    let size = |path: &Path| -> u64 { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
    let bytes = [
        size(&flat_values),
        size(&flat_presence),
        size(&dict_path),
        size(&ord_values),
        size(&ord_presence),
    ];

    let flat = ValueColumn::open(
        &flat_values,
        (!universal).then_some(flat_presence.as_path()),
        Access::Read,
    )?;
    let ordinals = ValueColumn::open(
        &ord_values,
        (!universal).then_some(ord_presence.as_path()),
        Access::Read,
    )?;

    let at = |f: f64| -> String { present[((present.len() as f64 * f) as usize).min(present.len() - 1)].to_string() };
    let mid = at(0.5);
    Ok(Both {
        column,
        presence: (!universal).then_some(presence),
        flat,
        ordinals,
        dict,
        entities,
        distinct: distinct.len(),
        eq_needles: EQ_POSITIONS.iter().map(|f| at(*f)).collect(),
        in_needles: IN_POSITIONS.iter().map(|f| at(*f)).collect(),
        prefix_needle: clip(&mid, column.prefix_len()).to_string(),
        contains_needle: mid_substring(&mid),
        bytes,
    })
}

/// The stride a scattered candidate takes. **Fixed, not derived from the cardinality**: a stride
/// that shrank as the candidate grew would vary the candidate's *shape* along a sweep whose only
/// intended variable is its size, and the run structure is what the flat `contains` scan's region
/// search keys off. Four is the design's own "scattered 25% principal" (§6.4).
const SCATTER_STRIDE: u32 = 4;

/// A candidate of `count` entities, either one centred run or every fourth entity of a centred
/// window. A scattered candidate is capped at a quarter of the corpus, which is what a fixed
/// stride means; the caller reads the cardinality back rather than assuming it.
///
/// Centred rather than leading: arXiv identifier styles change over the corpus's life, so a run
/// from entity 0 would select one style and measure a vocabulary the whole column does not have.
fn candidate(entities: u32, count: u32, scattered: bool) -> Bitmap {
    let mut b = Bitmap::new();
    if scattered {
        let take = count.min(entities / SCATTER_STRIDE);
        let start = (entities - take * SCATTER_STRIDE) / 2;
        for i in 0..take {
            b.add(start + i * SCATTER_STRIDE);
        }
    } else {
        let start = (entities - count.min(entities)) / 2;
        b.add_range(start..start + count.min(entities));
    }
    b.run_optimize();
    b
}

/// Run `f` `repeat` times, keeping the **minimum** — the probes' convention, and the right
/// statistic for a constant being extracted from a machine with other things on it.
fn best(repeat: usize, mut f: impl FnMut() -> u64) -> (f64, u64) {
    let mut ns = u64::MAX;
    let mut answer = 0u64;
    for _ in 0..repeat {
        let t = Instant::now();
        answer = f();
        ns = ns.min(t.elapsed().as_nanos() as u64);
    }
    (ns as f64 / 1e6, answer)
}

/// Ordinals whose key contains `needle`, by walking every key in the dictionary.
///
/// Every key is decoded and searched: front coding elides shared prefixes, so a substring can span
/// an elided prefix and a flat pass over the block bytes would miss it (§4.3, review N5).
fn broad_ordinals(dict: &SortedDict, needle: &str) -> Vec<u32> {
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let mut out = Vec::new();
    dict.walk(|ordinal, key| {
        if finder.find(key.as_bytes()).is_some() {
            out.push(ordinal);
        }
    })
    .expect("a dictionary this process just wrote decodes");
    out
}

/// Runs of a bitmap, bulk-read through the cursor.
struct RunIter<'a> {
    cursor: croaring::bitmap::BitmapCursor<'a>,
    buf: [croaring::RangeInclusive<u32>; 64],
    filled: usize,
    at: usize,
}

impl<'a> RunIter<'a> {
    fn new(bitmap: &'a Bitmap) -> Self {
        RunIter {
            cursor: bitmap.cursor(),
            buf: [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; 64],
            filled: 0,
            at: 0,
        }
    }

    fn next(&mut self) -> Option<(u32, u32)> {
        if self.at == self.filled {
            self.filled = self.cursor.read_many_ranges(&mut self.buf);
            self.at = 0;
            if self.filled == 0 {
                return None;
            }
        }
        let r = self.buf[self.at];
        self.at += 1;
        Some((r.start, r.last))
    }
}

/// The shipped scan's candidate traversal, transcribed: `(slot0, count, entity0)` per contiguous
/// run of the candidate inside the column's presence.
///
/// **A transcription rather than a call, and the reason is a measurement.** `for_each_slot_run` is
/// private and `ValueColumn::value_of` is the public way to reach a slot — but on a partial column
/// it costs a Roaring `rank` per entity, which measured **~0.9 µs per candidate entity on `doi`**,
/// nine times the dictionary probe the narrow route exists to pay. Driving the route through it
/// would have priced the harness's own addressing into the route's constant and reported a
/// regression that was the harness's. This is the same run-merge the scan uses — rank is affine
/// inside a presence run — so the routes below pay what a built one would.
fn for_each_slot_run(
    candidate: &Bitmap,
    presence: Option<&Bitmap>,
    len: usize,
    mut f: impl FnMut(usize, usize, u32),
) {
    match presence {
        None => {
            let bound = len as u32;
            let mut runs = RunIter::new(candidate);
            while let Some((start, last)) = runs.next() {
                let last = last.min(bound.saturating_sub(1));
                if start > last {
                    continue;
                }
                f(start as usize, (last - start) as usize + 1, start);
            }
        }
        Some(presence) => {
            let live = candidate.and(presence);
            let mut pres = RunIter::new(presence);
            let mut liv = RunIter::new(&live);
            let mut base: u64 = 0;
            let mut p = pres.next();
            let mut l = liv.next();
            while let (Some((ps, pl)), Some((ls, ll))) = (p, l) {
                if pl < ls {
                    base += u64::from(pl - ps) + 1;
                    p = pres.next();
                    continue;
                }
                if ll < ps {
                    l = liv.next();
                    continue;
                }
                let lo = ls.max(ps);
                let hi = ll.min(pl);
                let slot0 = (base + u64::from(lo - ps)) as usize;
                let count = (hi - lo) as usize + 1;
                if slot0 < len {
                    f(slot0, count.min(len - slot0), lo);
                }
                if ll <= pl {
                    l = liv.next();
                } else {
                    base += u64::from(pl - ps) + 1;
                    p = pres.next();
                }
            }
        }
    }
}

/// Matching entities accumulated in ascending order. The shipped scan's `Hits` also coalesces
/// consecutive stretches into ranges; this only buffers, which costs the *broad* route a little on
/// an unselective needle and the narrow route nothing measurable, since its per-entity work is a
/// dictionary probe two orders of magnitude larger.
struct Acc {
    buf: Vec<u32>,
    out: Bitmap,
}

impl Acc {
    fn new() -> Self {
        Acc {
            buf: Vec::with_capacity(1 << 16),
            out: Bitmap::new(),
        }
    }

    #[inline]
    fn push(&mut self, entity: u32) {
        self.buf.push(entity);
        if self.buf.len() == self.buf.capacity() {
            self.out.add_many(&self.buf);
            self.buf.clear();
        }
    }

    fn finish(mut self) -> Bitmap {
        self.out.add_many(&self.buf);
        self.out
    }
}

/// The ordinal column's values in slot order — what a built keyword route indexes.
fn ord_slice(column: &ValueColumn) -> &[u32] {
    match column.codes() {
        Codes::U32(v) => v,
        _ => unreachable!("the ordinal column is built as Codes::U32 above"),
    }
}

/// The narrow `contains` route: one dictionary probe per candidate entity.
fn narrow_contains(both: &Both, cand: &Bitmap, needle: &str) -> Bitmap {
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let ords = ord_slice(&both.ordinals);
    let dict = &both.dict;
    let mut scratch = Vec::new();
    let mut hits = Acc::new();
    for_each_slot_run(cand, both.presence.as_ref(), ords.len(), |slot0, count, e0| {
        for i in 0..count {
            let key = dict
                .key_of(ords[slot0 + i], &mut scratch)
                .expect("an ordinal read out of the column is in the dictionary");
            if finder.find(key.as_bytes()).is_some() {
                hits.push(e0 + i as u32);
            }
        }
    });
    hits.finish()
}

/// The ordinal set test, as a dense bitset over the dictionary rather than a binary search per
/// slot. **Bench-local, not shipped** — see the header.
fn ordinal_bitset_scan(both: &Both, cand: &Bitmap, ordinals: &[u32]) -> Bitmap {
    let mut set = vec![false; both.distinct];
    for o in ordinals {
        set[*o as usize] = true;
    }
    let ords = ord_slice(&both.ordinals);
    let mut hits = Acc::new();
    for_each_slot_run(cand, both.presence.as_ref(), ords.len(), |slot0, count, e0| {
        for i in 0..count {
            if set[ords[slot0 + i] as usize] {
                hits.push(e0 + i as u32);
            }
        }
    });
    hits.finish()
}

fn scalars(ordinals: &[u32]) -> Vec<Scalar> {
    ordinals.iter().map(|o| Scalar::Int(*o as i128)).collect()
}

/// A needle's ordinal, or `u32::MAX` for one the dictionary does not hold — an ordinal no slot
/// carries, which is how "an unresolved needle still scans" is expressed without a branch.
fn resolve(both: &Both, needle: &str) -> u32 {
    both.dict
        .resolve(needle)
        .expect("a dictionary this process just wrote decodes")
        .unwrap_or(u32::MAX)
}

/// One printed measurement line.
#[allow(clippy::too_many_arguments)]
fn row(
    column: &str,
    op: &str,
    shape: &str,
    cand: u64,
    scanned: u64,
    route: &str,
    ms: f64,
    matched: u64,
) {
    println!(
        "{column:<10} {op:<9} {shape:<11} {cand:>9} {scanned:>9} {route:<22} {ms:>9.2} {:>9.3} {matched:>10}",
        if scanned == 0 {
            0.0
        } else {
            ms * 1e6 / scanned as f64
        },
    );
}

fn header() {
    println!(
        "{:<10} {:<9} {:<11} {:>9} {:>9} {:<22} {:>9} {:>9} {:>10}",
        "column", "op", "candidate", "|cand|", "scanned", "route", "ms", "ns/ent", "matched"
    );
}

/// **Every keyword route must return exactly the flat scan's answer**, or the cells below compare
/// two different questions and the cheaper one is cheaper for the wrong reason.
///
/// Checked over the whole corpus as the candidate — the widest case, where a disagreement cannot
/// hide in an unvisited slot — for every needle the timed cells use, including the absent one. A
/// mismatch aborts: a wrong route's cost is not a measurement of anything.
fn verify(both: &Both) {
    let mut all = Bitmap::new();
    all.add_range(0..both.entities);
    all.run_optimize();
    let name = both.column.name();
    let same = |op: &str, route: &str, a: &Bitmap, b: &Bitmap| {
        assert!(
            a == b,
            "{name}: the keyword {route} route for `{op}` disagrees with the flat scan \
             ({} entities against {})",
            b.cardinality(),
            a.cardinality()
        );
    };

    let mut needles = both.eq_needles.clone();
    needles.push(ABSENT.to_string());
    for needle in &needles {
        let flat = both.flat.scan_text_eq(&all, needle);
        let ordinal = resolve(both, needle);
        let kw = both
            .ordinals
            .scan_num_eq(&all, Scalar::Int(ordinal as i128));
        same("eq", "scan_num_eq", &flat, &kw);
        let kw = both.ordinals.scan_eq(&all, AttrLocalId::new(ordinal));
        same("eq", "scan_eq", &flat, &kw);
    }

    let flat = both.flat.scan_text_prefix(&all, &both.prefix_needle);
    let r = both.dict.prefix_range(&both.prefix_needle).expect("decodable");
    let kw = both.ordinals.scan_range(
        &all,
        Some(tessera_filter::Endpoint {
            value: Scalar::Int(r.start as i128),
            inclusive: true,
        }),
        Some(tessera_filter::Endpoint {
            value: Scalar::Int(r.end as i128 - 1),
            inclusive: true,
        }),
    );
    same("prefix", "range+ordinal", &flat, &kw);

    let flat = both.flat.scan_text_in(&all, &both.in_needles);
    let ords: Vec<Scalar> = both
        .in_needles
        .iter()
        .map(|x| Scalar::Int(resolve(both, x) as i128))
        .collect();
    same("in", "scan_num_in", &flat, &both.ordinals.scan_num_in(&all, &ords));
    let ords: Vec<AttrLocalId> = both
        .in_needles
        .iter()
        .map(|x| AttrLocalId::new(resolve(both, x)))
        .collect();
    same("in", "scan_in", &flat, &both.ordinals.scan_in(&all, &ords));

    for needle in [both.contains_needle.as_str(), ABSENT] {
        let flat = both.flat.scan_text_contains(&all, needle);
        let matching = broad_ordinals(&both.dict, needle);
        let sc = scalars(&matching);
        same(
            "contains",
            "broad, shipped set scan",
            &flat,
            &both.ordinals.scan_num_in(&all, &sc),
        );
        same(
            "contains",
            "broad, ordinal bitset",
            &flat,
            &ordinal_bitset_scan(both, &all, &matching),
        );
        same(
            "contains",
            "narrow",
            &flat,
            &narrow_contains(both, &all, needle),
        );
    }
}

/// Cell 1: the four operators, both formats, at one candidate cardinality and two candidate shapes.
fn operators(both: &Both, repeat: usize) {
    let count = (both.entities as f64 * CANDIDATE_FRACTION) as u32;
    for scattered in [false, true] {
        let shape = if scattered { "scattered" } else { "contiguous" };
        let cand = candidate(both.entities, count, scattered);
        let scanned = both.flat.present_in(&cand).cardinality();
        let n = cand.cardinality();
        let c = both.column.name();

        // `eq`, averaged over the five positional needles plus the absent one. Each needle is run
        // against both formats before the next is built, so a machine that drifts drifts on both.
        for (label, needles) in [
            ("hit", both.eq_needles.clone()),
            ("miss", vec![ABSENT.to_string()]),
        ] {
            let mut flat_ms = 0.0;
            let mut num_ms = 0.0;
            let mut code_ms = 0.0;
            let mut flat_hits = 0;
            let mut kw_hits = 0;
            for needle in &needles {
                // The resolve is inside every timed region below: it is work the route does, once
                // per request. A miss resolves to no ordinal and still scans — §4.3's rule — which
                // `u32::MAX` expresses without a branch the scan could skip on.
                let (ms, h) = best(repeat, || both.flat.scan_text_eq(&cand, needle).cardinality());
                flat_ms += ms;
                flat_hits += h;
                let (ms, h) = best(repeat, || {
                    let ordinal = resolve(both, needle);
                    both.ordinals
                        .scan_num_eq(&cand, Scalar::Int(ordinal as i128))
                        .cardinality()
                });
                num_ms += ms;
                kw_hits += h;
                let (ms, _) = best(repeat, || {
                    let ordinal = resolve(both, needle);
                    both.ordinals
                        .scan_eq(&cand, AttrLocalId::new(ordinal))
                        .cardinality()
                });
                code_ms += ms;
            }
            // Mean milliseconds **per needle**, so `ns/ent` compares with the single-needle
            // operators below; `matched` is the total across the needle set.
            let k = needles.len() as f64;
            let op = if label == "hit" { "eq" } else { "eq-miss" };
            row(c, op, shape, n, scanned, "flat scan", flat_ms / k, flat_hits);
            row(c, op, shape, n, scanned, "ordinal: scan_num_eq", num_ms / k, kw_hits);
            row(c, op, shape, n, scanned, "ordinal: scan_eq", code_ms / k, kw_hits);
        }

        // `prefix`: a dictionary range, then a numeric range over the ordinals.
        let p = &both.prefix_needle;
        let (ms, h) = best(repeat, || both.flat.scan_text_prefix(&cand, p).cardinality());
        row(c, "prefix", shape, n, scanned, "flat scan", ms, h);
        let (ms, h) = best(repeat, || {
            let r = both
                .dict
                .prefix_range(p)
                .expect("a dictionary this process just wrote decodes");
            if r.is_empty() {
                // An empty range still scans, for the reason the `eq` miss does.
                return both
                    .ordinals
                    .scan_num_eq(&cand, Scalar::Int(u32::MAX as i128))
                    .cardinality();
            }
            both.ordinals
                .scan_range(
                    &cand,
                    Some(tessera_filter::Endpoint {
                        value: Scalar::Int(r.start as i128),
                        inclusive: true,
                    }),
                    Some(tessera_filter::Endpoint {
                        value: Scalar::Int(r.end as i128 - 1),
                        inclusive: true,
                    }),
                )
                .cardinality()
        });
        row(c, "prefix", shape, n, scanned, "range+ordinal", ms, h);

        // `in`: k resolves, then the ordinal set scan.
        let needles = both.in_needles.clone();
        let (ms, h) = best(repeat, || both.flat.scan_text_in(&cand, &needles).cardinality());
        row(c, "in", shape, n, scanned, "flat scan", ms, h);
        let (ms, h) = best(repeat, || {
            let ords: Vec<Scalar> = needles
                .iter()
                .map(|x| Scalar::Int(resolve(both, x) as i128))
                .collect();
            both.ordinals.scan_num_in(&cand, &ords).cardinality()
        });
        row(c, "in", shape, n, scanned, "ordinal: scan_num_in", ms, h);
        let (ms, h) = best(repeat, || {
            let ords: Vec<AttrLocalId> = needles
                .iter()
                .map(|x| AttrLocalId::new(resolve(both, x)))
                .collect();
            both.ordinals.scan_in(&cand, &ords).cardinality()
        });
        row(c, "in", shape, n, scanned, "ordinal: scan_in", ms, h);

        // `contains`: the flat scan against both keyword routes, on the corpus-derived needle and
        // on an absent one.
        for (op, needle) in [
            ("contains", both.contains_needle.clone()),
            ("cont-miss", ABSENT.to_string()),
        ] {
            let (ms, h) = best(repeat, || {
                both.flat.scan_text_contains(&cand, &needle).cardinality()
            });
            row(c, op, shape, n, scanned, "flat scan", ms, h);

            let (walk_ms, keys) = best(repeat, || broad_ordinals(&both.dict, &needle).len() as u64);
            row(c, op, shape, n, scanned, "broad: walk only", walk_ms, keys);
            let ords = broad_ordinals(&both.dict, &needle);
            let sc = scalars(&ords);
            let (ms, h) = best(repeat, || both.ordinals.scan_num_in(&cand, &sc).cardinality());
            row(c, op, shape, n, scanned, "broad: +set scan", walk_ms + ms, h);
            let (ms, h) = best(repeat, || {
                ordinal_bitset_scan(both, &cand, &ords).cardinality()
            });
            row(c, op, shape, n, scanned, "broad: +bitset", walk_ms + ms, h);

            let (ms, h) = best(repeat, || narrow_contains(both, &cand, &needle).cardinality());
            row(c, op, shape, n, scanned, "narrow: key_of/cand", ms, h);
        }
    }
}

/// Cell 2: where the two `contains` routes cross, swept over candidate cardinality.
fn crossover(both: &Both, repeat: usize) {
    let c = both.column.name();
    let needle = both.contains_needle.clone();
    let ords = broad_ordinals(&both.dict, &needle);
    let sc = scalars(&ords);
    for scattered in [false, true] {
        let shape = if scattered { "scattered" } else { "contiguous" };
        let mut last = 0u64;
        for ratio in CROSSOVER_RATIOS {
            let count = ((both.distinct as f64 * ratio) as u32).min(both.entities);
            if count == 0 {
                continue;
            }
            let cand = candidate(both.entities, count, scattered);
            let n = cand.cardinality();
            // A scattered candidate saturates at a quarter of the corpus, so the ratios above that
            // repeat a cardinality already measured rather than adding a point.
            if n == last {
                continue;
            }
            last = n;
            let scanned = both.flat.present_in(&cand).cardinality();
            let (flat, _) = best(repeat, || {
                both.flat.scan_text_contains(&cand, &needle).cardinality()
            });
            let (walk, _) = best(repeat, || broad_ordinals(&both.dict, &needle).len() as u64);
            let (set, _) = best(repeat, || both.ordinals.scan_num_in(&cand, &sc).cardinality());
            let (bits, _) = best(repeat, || {
                ordinal_bitset_scan(both, &cand, &ords).cardinality()
            });
            let (narrow, _) = best(repeat, || narrow_contains(both, &cand, &needle).cardinality());
            // The ratio as *realised*, not as asked for: the cap above can hold `n` below the
            // sweep point, and reporting the request would misplace the crossover.
            let ratio = n as f64 / both.distinct as f64;
            println!(
                "{c:<10} {shape:<11} {n:>9} {scanned:>9} {ratio:>6.2} {flat:>9.2} {:>9.2} {:>9.2} {narrow:>9.2} {:>10}",
                walk + set,
                walk + bits,
                if narrow < walk + bits { "narrow" } else { "broad" },
            );
        }
    }
}

fn read_snapshot(
    path: &PathBuf,
    limit: usize,
) -> std::io::Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let (mut ids, mut submitters, mut dois) = (Vec::new(), Vec::new(), Vec::new());
    for line in reader.lines() {
        if ids.len() >= limit {
            break;
        }
        let line = line?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let f = |k: &str| -> String {
            v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
        };
        ids.push(f("id"));
        submitters.push(f("submitter"));
        dois.push(f("doi"));
    }
    Ok((ids, submitters, dois))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut snapshot: Option<PathBuf> = None;
    let mut limit = 2_400_000usize;
    let mut repeat = 5usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--snapshot" => snapshot = args.next().map(PathBuf::from),
            "--limit" => limit = args.next().and_then(|v| v.parse().ok()).unwrap_or(limit),
            "--repeat" => repeat = args.next().and_then(|v| v.parse().ok()).unwrap_or(repeat),
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }
    let Some(snapshot) = snapshot else {
        return Err("--snapshot <arxiv-metadata-oai-snapshot.json> is required".into());
    };
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — every figure here is meaningless. Use --release.");
    }

    eprintln!("reading {} ...", snapshot.display());
    let t0 = Instant::now();
    let (ids, submitters, dois) = read_snapshot(&snapshot, limit)?;
    eprintln!(
        "read {} records in {:.1} s (snapshot order = submission order = entity order)",
        ids.len(),
        t0.elapsed().as_secs_f64()
    );

    let dir = std::env::temp_dir().join(format!("tessera-utf8-fence-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    let built = [
        build(Column::Id, &ids, &dir)?,
        build(Column::Submitter, &submitters, &dir)?,
        build(Column::Doi, &dois, &dir)?,
    ];
    for b in &built {
        verify(b);
    }
    eprintln!("every keyword route agrees with the flat scan, on every needle, over the whole corpus");

    println!("\n### the needles, fixed by position before anything was timed\n");
    println!(
        "{:<10} {:>9} {:>9} {:<26} {:<10} {:<8}",
        "column", "entities", "distinct", "eq[0]", "prefix", "contains"
    );
    for b in &built {
        println!(
            "{:<10} {:>9} {:>9} {:<26} {:<10} {:<8}",
            b.column.name(),
            b.entities,
            b.distinct,
            b.eq_needles[0],
            b.prefix_needle,
            b.contains_needle
        );
    }

    println!("\n### stored bytes: the same column both ways, on disk, through the shipped writers\n");
    println!(
        "{:<10} {:>9} {:>12} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9} {:>7}",
        "column",
        "present",
        "flat values",
        "flat pres",
        "dict.bin",
        "ordinals",
        "kw pres",
        "flat B/e",
        "kw B/e",
        "ratio"
    );
    for b in &built {
        let present = b
            .presence
            .as_ref()
            .map(|p| p.cardinality())
            .unwrap_or(b.entities as u64);
        let flat = b.bytes[0] + b.bytes[1];
        let kw = b.bytes[2] + b.bytes[3] + b.bytes[4];
        println!(
            "{:<10} {present:>9} {:>12} {:>10} {:>10} {:>10} {:>10} {:>9.2} {:>9.2} {:>7.2}",
            b.column.name(),
            b.bytes[0],
            b.bytes[1],
            b.bytes[2],
            b.bytes[3],
            b.bytes[4],
            flat as f64 / present as f64,
            kw as f64 / present as f64,
            flat as f64 / kw as f64,
        );
    }
    println!(
        "\nPresence is written for both formats and is identical in both, so it appears on both\n\
         sides rather than being excluded from either. ratio = flat / keyword: above 1 the family\n\
         is the smaller artefact."
    );

    println!(
        "\n### cell 1: the four operators, one candidate of {:.0}% of the corpus, both shapes\n",
        CANDIDATE_FRACTION * 100.0
    );
    header();
    for b in &built {
        operators(b, repeat);
    }
    println!(
        "\n`scanned` is the candidate's entities the column holds a value for, which is what both\n\
         formats walk and therefore the denominator of `ns/ent`. For `eq` and `in`, `ms` is the\n\
         mean over the needle set and `matched` its total. `broad: walk only` reports keys, not\n\
         entities, and its milliseconds are included in the two rows below it."
    );

    println!("\n### cell 2: the `contains` crossover, swept over candidate cardinality\n");
    println!(
        "{:<10} {:<11} {:>9} {:>9} {:>6} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "column", "candidate", "|cand|", "scanned", "C/D", "flat ms", "broad set", "broad bit", "narrow", "cheaper"
    );
    for b in &built {
        crossover(b, repeat);
    }
    println!(
        "\nC/D = |candidate| / |dictionary|, the quantity §4.3's route rule compares. `cheaper`\n\
         reads narrow against the bitset broad route, which is the broad route as it would be\n\
         built; the `broad set` column is the same route assembled from the shipped `scan_num_in`."
    );

    for entry in std::fs::read_dir(&dir)? {
        let _ = std::fs::remove_file(entry?.path());
    }
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}
