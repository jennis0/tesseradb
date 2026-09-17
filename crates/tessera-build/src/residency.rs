//! **What the build holds when it is not in the batch loop** — and the refusal that fires before
//! the first pass rather than after the second hour.
//!
//! # Why this exists
//!
//! [`crate::pipeline::plan_build`] models the signature batch loop: the packed bucket, the records,
//! the starts, the entity map, the per-term counters, the join chunk and a fixed slack. That model
//! is complete for the stages it covers and it is the whole of what `--memory-budget` reached.
//!
//! The stages *after* the loop were outside it entirely, and they are the larger peak. The
//! campaign at `probes/2026-08-22-artifact-serving-e2e/` measured it: builds at 10⁸ and 2.5×10⁸
//! points were OOM-killed at **47.3–47.6 GB** with `--memory-budget 12g`, three runs, one number —
//! the peak was the machine and the flag reached none of it. Every kill landed immediately after
//! the attribute pass's summary line, which is where the entity-order tail below is fully resident.
//!
//! # What is resident there
//!
//! From the attribute pass to the segment write the build holds the **published memberships** in
//! the store, as Roaring, and nothing else this model charges. The two structures that resolve a
//! source id — the **sorted source ids** at 8 bytes an item and the **ordinal→entity map** at 4 —
//! are both files under `.build-tmp/` and are charged to the disk below rather than to memory.
//!
//! **The publication is not a batch.** The loop's residency shrinks when the stride does; this
//! does not shrink at all, because a member table is its own size. So for that one term the
//! honest answer is a refusal that names the number, which is what this module computes and
//! `plan_build` acts on.
//!
//! # What moved off the heap, and is still counted
//!
//! The **layer member tables** were the third item on that list until 2026-08-30: one `Vec<u64>` of
//! source ids per artifact as the plan was read, every one of them live from the first member row
//! to the last level published. They are sorted runs and a merged table under `.build-tmp/` now
//! (`layers.rs`), read back one artifact at a time, so what the plan holds is a spill budget and
//! what the disk holds is the corpus.
//!
//! The **sorted source ids** were the largest item on it until 2026-09-10: 8 bytes an item of
//! anonymous memory, 26.0 GiB at the GBIF rung's 3.50×10⁹ items, and 52.1 GiB while pass one built
//! them, because each view's ids were read into a vector of their own and then concatenated into a
//! second. Both are gone — the union is one array of the final length, filled segment by segment,
//! and the array is a file under `.build-tmp/` ([`crate::pipeline::SourceIds`]). Measured at
//! 10⁹ items: a 14.96 GiB peak became 7.55 GiB of page cache over a flat 228 MiB of anonymous
//! memory (`probes/2026-09-10-source-ids-memory/`).
//!
//! On a corpus that numbers its rows the array is not written at all. Pass one proves the union is
//! one unbroken range from a presence bitmap a sixty-fourth of its size, and an ordinal is then a
//! subtraction — so this term is zero, and [`IdShape::slots`] is what says which build is which.
//!
//! Every **declared column in entity order** was the other half of this list and the larger half of
//! the campaign's kills: a fixed-width type at its own width, a `text`, `keyword` or `utf8` one at a
//! `String` *per entity* — 24 bytes of header before a character was stored — plus a presence bit
//! each. They are now files under `.build-tmp/`, mapped rather than held ([`crate::column`]), so
//! they cost the machine page cache the kernel may evict and not memory it must have.
//!
//! The **published memberships** were on that list until 2026-09-02 and were the largest item on it
//! at a hierarchy rung: one Roaring bitmap per artifact in the store, live from the layers stage to
//! the artifact pass four stages later, **+1.2 GB of anonymous memory at the 10⁷ MedCPT sample**
//! with 471,778,374 closed MeSH member rows and ~47 GB extrapolated at 10⁸
//! (`probes/2026-09-02-mapped-memberships/README.md`). They are read back through the packed
//! extent `layers.rs` has just written and fsynced — one write, no second format — so what stays on
//! the heap is a Roaring container's descriptor and the members themselves are page cache. The
//! publication's own window still holds them ([`BYTES_PER_MEMBER_ENTRY`]); nothing after it does.
//!
//! The segment's **row-order** tail went the same way and further: eight render columns at
//! 7.4×10⁷ rows were ~2.4 GB of `Vec`, built by `push` immediately after the entity-order columns
//! stopped being heap, and then a mapped file per column written at a scattered row index. It is
//! now neither ([`crate::assembly`]): each render column is read in entity order out of one
//! `(entity, row)` bucket at a time, pushed to a `(row, value)` partition, and each row bucket's
//! window is written straight into `columns.arrow`'s own buffer. ⊘ The heap form was never a term
//! of this model — it lived in the segment write, not in the entity-order window this module
//! covers — so what the partitions are charged here is the assembly's constant, and the bytes they
//! spill are charged to the disk model's assembly phase.
//!
//! They are still modelled, and still printed, as **mapped** terms: an operator whose disk is the
//! constraint has the same right to see the number as one whose memory is. What changed is that
//! [`Residency::total`] — the figure `--memory-budget` is compared against — leaves them out. A
//! model that kept charging them would refuse builds that now fit, which is the failure mode of
//! carrying a cost model past the thing it modelled.
//!
//! **[`disk`] is the other half, and the disk pre-flight is its reader.** It carries every mapped
//! term below and adds what lives outside this window: the pair spills, the geometry, the join's
//! staging buffer, the keyword dictionaries' scratch, the row spaces' own files, and the bundle,
//! which nothing releases. Each term names the [`Phase`]s it stands through and the forecast is the
//! largest phase, because two structures whose lifetimes do not overlap cost the larger and not the
//! sum.
//!
//! # What the numbers are, and what they are not
//!
//! Every term below is arithmetic over things known before the first pass: the item count, the
//! declared schema, a member source's declared key values (its footer's), and a string column's
//! bytes an item (a sample of its row groups — see [`ColumnCost`] for why the footer will not say).
//!
//! ⊘ **The memory figure is a lower bound, and measured to be about half the real peak.**
//! Transients inside a stage — a Parquet decode buffer, an analyser's scratch, the allocator's own
//! slack — are not enumerated, and [`SLACK`] is one constant standing in for all of them. The one build it has been
//! checked against ([`tests::the_model_is_checked_against_an_observed_peak`], 10⁶ items, one `text`
//! column, 10⁶ member rows) read **190 MiB against an observed 407 MiB**, and the trace shows why:
//! the process was already at 331 MiB in its first stage, reading the points file, before a single
//! term below existed.
//!
//! ⊘ **The largest transient this leaves out is the member merge's own vector**, and it is not a
//! function of anything the plan knows. `layers::merge_member_runs` holds every source id of the
//! artifact it is emitting as one `Vec<u64>`, because a source id's entity is not a monotone
//! function of it and the membership has to be in hand to be sorted; two later passes decode the
//! same artifact again. The denominator is the **largest single artifact's** member count, which
//! the member file's footer does not carry and which nothing short of a pass over the file would
//! establish. At GBIF's kingdom level that vector is 22.5 GB (2.81×10⁹ members at 8 bytes) and at
//! its family level 1.4 GB, which is why that corpus's taxonomy starts at family. No term below
//! covers it.
//!
//! So the bound is used the way a lower bound can be: **over budget refuses**, because a lower bound
//! that already exceeds the budget settles it, and the band below refuses nothing and prints the
//! numbers instead. What this buys is the difference between *refused in the first second with the
//! arithmetic printed* and *killed at hour two with nothing written* — and it does not claim to
//! catch every build that will not fit. At the campaign's own corner it catches the headline one:
//! 2.5×10⁸ points over the generator's declaration models at tens of gigabytes against the 12 GB
//! budget those runs passed, where 10⁸ under an auto-derived budget still slips through.
//!
//! ⊘ **The disk figure is neither a bound nor a measurement, and it decides nothing.** Most terms
//! are arithmetic over a fixed width and are exact; several are stated ceilings a term of the right
//! shape sits far below (`postings.arrow` over a relation whose terms encode as runs — GBIF's 253
//! country terms are 33,714 bytes against the hundreds of megabytes charged); and four are
//! estimates that a corpus shape can exceed, each saying so at itself: the record blob's blocks at
//! half the characters ([`EXTENT_SHARE`]), a published member entry at three bytes
//! ([`MAPPED_BYTES_PER_MEMBER_ENTRY`]), the member spill's pair of files at four
//! ([`SPILLED_BYTES_PER_MEMBER_ENTRY`]), and a text index at 35% of its column
//! ([`TEXT_INDEX_SHARE_PERCENT`]). Each term's own doc says whether it is an exact width, a
//! ceiling or an estimate, and a term that is not exact says so where the forecast prints it.
//!
//! ⊘ **Measured at 1.66, 1.49, 1.39 and 1.34 times the peak**, at 16.3×10⁶, 30.1×10⁶, 64.7×10⁶ and
//! 125.8×10⁶ GBIF occurrences: this crate's own binary against the peaks sampled in
//! `probes/2026-09-10-build-disk/`, re-run in `probes/2026-09-11-disk-forecast/`, which also holds
//! what that corpus does not exercise. The margin falls with the corpus because what is left of it
//! is constants. On a corpus with three indexed `text` columns the model reached none of the
//! 176.81 B/item — 23% of that bundle — that the token index and the dictionaries cost, until they
//! became terms.
//!
//! **So the pre-flight prints it and warns; it does not refuse** (`crate::pipeline::plan_build`).
//! A figure that can be wrong in either direction is not one to hold a door with, and a build that
//! runs out of disk is recoverable: it writes no `CURRENT`, publishes no identity, mints no id that
//! survives, and its partial prefix is swept on the way out.

use tessera_spatial::ScalarType;

/// The unenumerated transients: decode buffers, a stage's scratch, the allocator's slack. The
/// batch loop's own model carries a constant of the same size and for the same reason.
pub(crate) const SLACK: u64 = 64 << 20;

/// What a string value costs outside its characters: the entity-indexed arena offset in
/// [`crate::column::EntityColumn`], plus the arena record's own header.
///
/// Eight bytes of offset always. The header is eight more for a `text` column — the entity and
/// the length, which is what makes the arena readable in its own order — and **nothing** for the
/// rest, whose length is packed into the offset word beside the offset (`column.rs`,
/// `RecordShape`). The characters ride in the arena and are counted as its payload; the `String`
/// header this replaced was 24 bytes per entity, paid before a character was stored.
fn arena_offset(ty: ScalarType) -> u64 {
    match ty {
        ScalarType::Text => 8 + 8,
        _ => 8,
    }
}

/// The windows the disk pre-flight takes the largest of, in build order.
///
/// **A build's files do not all stand at once.** The pair buckets are consumed by the batch loop,
/// a declared column is unlinked at its last reader, the member table goes back at the publication
/// — and the bundle's own bytes only accumulate. Two structures whose lifetimes do not overlap cost
/// the larger and not the sum, so the forecast is a maximum over these six and every term below
/// says which of them it is on the disk for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Pass one to the pairs pack: the sorted source ids, the pair buckets, and each view's
    /// geometry as it is read.
    Spill,
    /// The batch loop and the postings write: the term bands, the anchor geometry, the
    /// ordinal→entity map, and the first of the bundle's own files.
    Bands,
    /// The attribute join and the layer publication — **the model's largest phase below 10⁸ items**
    /// on every corpus measured, and within 3% of the index phase beside it. Every declared column
    /// filling, the join's staging buffer, the source ids, and the member spill.
    Join,
    /// The filter postings, the keyword dictionaries and the text index: the columns full, their
    /// dictionaries' scratch beside them, and the value columns being written. **The model's
    /// largest phase at 125.8×10⁶ GBIF occurrences**, which is where the measurement puts the peak
    /// (`probes/2026-09-11-disk-forecast/`).
    Index,
    /// The record blob. A column with no blob row went back to the disk at the end of the phase
    /// above, so what stands here is the blob-resident and render columns and the blob itself.
    Blob,
    /// The row spaces, the segment write and the artifact pass: the entity-space geometry, the
    /// row-order render tail, and the whole finished bundle.
    Assemble,
}

impl Phase {
    pub(crate) const ALL: [Phase; 6] = [
        Phase::Spill,
        Phase::Bands,
        Phase::Join,
        Phase::Index,
        Phase::Blob,
        Phase::Assemble,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Phase::Spill => "spill",
            Phase::Bands => "band",
            Phase::Join => "join",
            Phase::Index => "index",
            Phase::Blob => "blob",
            Phase::Assemble => "assembly",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which phases a term's bytes are on the disk for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Phases(u8);

impl Phases {
    pub(crate) const SPILL: Phases = Phases(Phase::Spill.bit());
    pub(crate) const BANDS: Phases = Phases(Phase::Bands.bit());
    pub(crate) const JOIN: Phases = Phases(Phase::Join.bit());
    pub(crate) const INDEX: Phases = Phases(Phase::Index.bit());
    pub(crate) const BLOB: Phases = Phases(Phase::Blob.bit());
    pub(crate) const ASSEMBLE: Phases = Phases(Phase::Assemble.bit());

    /// Both, for a term standing through two windows.
    pub(crate) const fn and(self, other: Phases) -> Phases {
        Phases(self.0 | other.0)
    }

    /// This phase and every one after it — what the bundle's own files take, nothing releasing
    /// them once they are written.
    pub(crate) const fn onwards(self) -> Phases {
        // Every bit at or above the lowest one set. A `Phases` naming two phases is not a range,
        // and `onwards` is only ever built from a single one.
        Phases(!(self.0 - 1))
    }

    fn holds(self, phase: Phase) -> bool {
        self.0 & phase.bit() != 0
    }
}

/// One named term of the residency, so a refusal prints where the bytes are rather than a total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Term {
    pub what: String,
    pub bytes: u64,
    /// A file under `.build-tmp/` or in the bundle, rather than anonymous memory — reported, but
    /// not charged against the memory budget.
    pub mapped: bool,
    /// The disk phases these bytes stand through. Meaningless for an anonymous term, which the
    /// memory model reads as one window.
    pub phases: Phases,
    /// **A constant of the code rather than a rate over the corpus.** A partition's writer buffers
    /// and the bucket the key type bounds, the Morton histogram, the allocator slack: these are the
    /// terms that stop the anonymous total being linear in `n`, and the two tests that assert what
    /// the model's shape is need to tell them apart from the terms that are a rate.
    pub constant: bool,
}

/// The entity-order residency, term by term.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Residency {
    pub terms: Vec<Term>,
}

impl Residency {
    /// What the build asks the *machine* for: the anonymous terms only. A mapped term is page cache
    /// and is reported rather than charged (see the module docs).
    pub fn total(&self) -> u64 {
        self.terms
            .iter()
            .filter(|t| !t.mapped)
            .map(|t| t.bytes)
            .sum()
    }

    /// What the build asks the **disk** for in one phase: the mapped terms standing through it.
    /// The counterpart of [`Self::total`], and what the disk pre-flight takes the maximum of.
    pub fn at(&self, phase: Phase) -> u64 {
        self.terms
            .iter()
            .filter(|t| t.mapped && t.phases.holds(phase))
            .map(|t| t.bytes)
            .sum()
    }

    /// The largest phase, and what it comes to.
    pub fn peak(&self) -> (Phase, u64) {
        Phase::ALL
            .iter()
            .map(|&phase| (phase, self.at(phase)))
            .max_by_key(|&(_, bytes)| bytes)
            .expect("Phase::ALL is not empty")
    }

    /// One phase's mapped terms as one line each, largest first — the form a disk refusal prints.
    pub fn describe_phase(&self, phase: Phase) -> String {
        let mut terms: Vec<&Term> = self
            .terms
            .iter()
            .filter(|t| t.mapped && t.phases.holds(phase) && t.bytes > 0)
            .collect();
        terms.sort_by_key(|t| std::cmp::Reverse(t.bytes));
        terms
            .iter()
            .map(|t| format!("\n  {:>9} MiB  {}", t.bytes >> 20, t.what))
            .collect()
    }

    /// The terms as one line each — the form a refusal prints. **Charged first, largest first
    /// within each half**, because the operator's next move is to drop or narrow whatever is at the
    /// top of what they are being refused for; the mapped terms follow, marked, because they are
    /// the disk the same build wants.
    pub fn describe(&self) -> String {
        let mut terms = self.terms.clone();
        terms.sort_by_key(|t| (t.mapped, std::cmp::Reverse(t.bytes)));
        terms
            .iter()
            .filter(|t| t.bytes > 0)
            .map(|t| {
                let mapped = if t.mapped { " (mapped)" } else { "" };
                format!("\n  {:>9} MiB{mapped}  {}", t.bytes >> 20, t.what)
            })
            .collect()
    }
}

/// What one declared column costs per entity, and where the variable part came from.
///
/// `payload_bytes` is the **characters** this build's `n` items carry in the column: the mean bytes
/// a row of the Parquet source holds, measured over a sample of its row groups
/// ([`sampled_bytes_per_item`]), times `n`.
///
/// ⊘ **It was the column's uncompressed size in the Parquet footer until 2026-09-10, and that
/// figure is not the characters.** Parquet's uncompressed size is the *encoded* page size, so a
/// dictionary-encoded column of repeated strings reads as its indices and its dictionary: GBIF's
/// `scientificname` footers say 7.22 bytes a row where the values are 31.22, and `specieskey`'s say
/// 2.14 where they are 6.60 (measured over 125,789,091 rows). At the GBIF rung that one term put
/// the forecast 104 GB under the arena the build then filled. The footer is still the fallback for
/// a file the sample cannot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnCost {
    pub ty: ScalarType,
    pub payload_bytes: u64,
    /// Whether the join spills this column's characters as record-blob extents rather than filling
    /// an entity-ordered arena with them ([`crate::pipeline::takes_extents`]). The two shapes cost
    /// different things and the declaration is what decides between them, so the pre-flight asks
    /// the pass's own predicate rather than the type.
    pub extents: bool,
    /// What the record-blob rows carrying this column's values cost beyond their characters, where
    /// this column spills its extents ([`record_framing_bytes`]). Zero for a column that keeps its
    /// arena, whose values are not framed until the record blob itself.
    pub framing_bytes: u64,
    /// Whether this column is the one a text index is built over — which costs the build a second
    /// set of files beside the column itself, and costs it them at the same time.
    pub text_index: bool,
    /// Whether the segment write gathers this column into `columns.arrow` — which is what opens a
    /// `(row, value)` partition for it in the assembly (`crate::assembly::RenderLane`). Read from
    /// the declaration rather than inferred from [`Self::phases`]: a blob-resident column stands
    /// through the same window and has no lane.
    pub render: bool,
    /// The windows this column's storage stands through, which is **its last reader and not the
    /// release stage**: a `render` column is read by the segment write, a blob-resident one by the
    /// record blob, and a column that is neither has met its last reader when the filter postings
    /// end (`pipeline::write_filter_postings`).
    pub phases: Phases,
}

/// Whether a column's values are characters rather than a fixed width — which is what makes them a
/// term of their own rather than part of the column's slot size.
///
/// Where those characters *land* differs by the column's readers and is priced at each term: a
/// column some pass reads at an entity holds them in [`crate::column::EntityColumn`]'s arena, and
/// one the record blob alone reads spills them as record-blob extents instead
/// ([`crate::pipeline::takes_extents`], [`crate::extents`], [`EXTENT_SHARE`]). The attribute join
/// stages every string type in an arena of its own whichever route it takes.
fn carries_characters(ty: ScalarType) -> bool {
    matches!(
        ty,
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text
    )
}

/// What a spilled column's extents cost against the column's Parquet payload: **one half**.
///
/// The extents are the same 256 KiB zstd blocks the base blob is cut into, and the base blob at
/// the 10⁸ PaperSeek rung measured 44.77 GB against 128 GiB of prose — 2.9×
/// (`probes/2026-09-04-rung-4-whole/breakdown.txt`). Half is charged rather than a 2.9th because
/// that ratio is one corpus's prose at one operating point. A keyword column compresses harder
/// still where its values repeat: GBIF's `scientificname` spilled 1.63 GB of extents over 3.93 GB
/// of characters, 0.42×, and its blocks alone 0.27× (measured,
/// `probes/2026-09-10-blob-resident-strings/`).
///
/// ⊘ **On the characters alone this is an estimate and not a ceiling.** A compression ratio has no
/// lower bound at one half. Measured at 0.251 on GBIF's `scientificname` and 0.345 on PaperSeek
/// prose, but at **0.567 to 0.750 on high-entropy short values** — random hex, base64 and
/// lowercase at 12 to 32 characters, zstd level 3 over 256 KiB blocks — and at 0.634 on GeoNames'
/// names. What bounds the error is that zstd does not expand incompressible input, so the blocks
/// are at most the characters and half of them reads at most 2× low.
///
/// **What closes most of that gap is the framing charged beside them**
/// ([`record_framing_bytes`]): a short value's row costs 15 bytes of frame around 8 to 13 of
/// characters, so half of the two together is above every measured shape but one. GeoNames' 13.5
/// characters a name compress to 8.6 bytes and are charged 14.2; 12 random lowercase characters
/// compress to 7.2 and are charged 13.5. The one shape still under-read is a value of 30
/// characters or more that does not compress at all — 32 random base64 characters are 24 bytes
/// against a charge of 23.5, 2% low.
const EXTENT_SHARE: u64 = 2;

/// What one record-blob row costs beyond its characters: **3 bytes a row and 3 a field**, plus a
/// `u32` length on each `utf8` one (`tessera_filter::record` — an entity gap and a row length,
/// then `tag u16 | kind u8 | value` a field).
///
/// Charged over the blob-resident columns and their rows because for a short value it is more
/// than the value: an 8-character key in one `utf8` field is 10 bytes of framing against 8 of
/// characters. It rides through the same zstd the blocks do, so it is charged at [`EXTENT_SHARE`]
/// with them.
fn record_framing_bytes(schema: &crate::config::Schema, rows: u64) -> u64 {
    let fields: u64 = schema
        .attributes
        .iter()
        .filter(|a| crate::pipeline::blob_resident(schema, a))
        .map(|a| {
            if carries_characters(a.ty) {
                RECORD_UTF8_FIELD_BYTES
            } else {
                RECORD_FIELD_BYTES
            }
        })
        .sum();
    if fields == 0 {
        return 0;
    }
    rows.saturating_mul(RECORD_ROW_BYTES + fields)
}

/// What a record-blob row costs the block around its fields: the entity gap the block header
/// carries for it and the length the row itself carries, both LEB128 varints. One byte each for a
/// row under 128 bytes whose entity follows its predecessor's within 128, charged at three so a
/// wider corpus is not under-charged.
const RECORD_ROW_BYTES: u64 = 3;

/// `tag u16 | kind u8` before a record-blob field's value.
const RECORD_FIELD_BYTES: u64 = 3;

/// What one **extent** row costs beyond its characters, for `rows` of one spilled column.
///
/// An extent row carries that column alone (`crate::extents`), so it is one row's framing and one
/// `utf8` field — 10 bytes around a value an unindexed `keyword` column can hold in 8. Charged for
/// every column the routing spills and not for the type: a `keyword` or `utf8` column the record
/// blob alone reads spills the same extents a `text` column does.
fn extent_framing_bytes(rows: u64) -> u64 {
    rows.saturating_mul(RECORD_ROW_BYTES + RECORD_UTF8_FIELD_BYTES)
}

/// [`RECORD_FIELD_BYTES`] and the `byte_len u32` a `utf8` value carries in front of its characters.
const RECORD_UTF8_FIELD_BYTES: u64 = RECORD_FIELD_BYTES + 4;

/// What `pairs.parquet` costs a pair, in tenths of a byte.
///
/// `(entity_id u64, term_id u32)` under `DELTA_BINARY_PACKED` and Snappy with no dictionary
/// (`tessera_store::pairs`). Entities ascend within a term and reset at each term boundary, so a
/// relation of short terms makes every mini-block pay for its outlier — and the delta is bounded
/// by the entity space, which I9 bounds at 2³². **The encoding therefore saturates at about 4.2
/// bytes a pair**, which is what is charged: measured 4.185 and 4.176 at the corner (10⁶ terms of
/// 10 and 10⁷ terms of 1 over a 4×10⁹ entity space) and 0.785 to 3.546 on every shape below it.
/// Four was charged as a ceiling before and is exceeded by 4–5% at the corner.
const ORACLE_BYTES_PER_PAIR_TENTHS: u64 = 42;

/// What a `text` column's **token index** costs against its characters, in hundredths:
/// `postings.arrow` and `dict.bin` together.
///
/// ⊘ **An estimate above every measurement, not a ceiling.** Measured on the 10⁷ MedCPT sample:
/// 0.166 of the characters on `abstract` (1.365 GB over 8.21 GB), 0.282 on `title`, 0.198 on
/// `mesh_major`; and the text index's sorted runs, which encode the same `(token, entity)` pairs
/// before they are packed, measured 0.215 on 7.4×10⁷ Overture names. Charged a quarter above the
/// largest of them, on [`MAPPED_BYTES_PER_MEMBER_ENTRY`]'s discipline.
///
/// **A column of many short distinct tokens exceeds it.** A posting is about four bytes a
/// `(token, entity)` pair whatever the token is, so a column of three-character tokens spends
/// about as many bytes on its index as on its characters. Nothing in the declaration says which a
/// column will be.
const TEXT_INDEX_SHARE_PERCENT: u64 = 35;

/// What one `dict.bin` entry costs beyond the characters it holds, in tenths of a byte: two
/// varints a key — the shared prefix length and the suffix length — and 8 bytes of restart offset
/// a block of 16 keys (`tessera_filter::dict`'s format and its `DEFAULT_RESTART_INTERVAL`).
///
/// Charged beside the characters rather than folded into them, because for a column of short
/// distinct values it is most of the file: 12-character keys are 2.5 bytes of entry against 12 of
/// suffix, and a term reading the characters alone as a ceiling is under the file wherever the
/// values do not share prefixes. Front coding removes bytes from a suffix and adds none, so the
/// characters bound the suffixes whatever the values are.
///
/// ⊘ A key of 128 bytes or more spends a second byte on each of the two varints. The same key's
/// own characters cover that many times over.
const DICT_BYTES_PER_KEY_TENTHS: u64 = 25;

/// The count at or below which a term's postings are a raw `u32` array rather than Roaring
/// (`tessera_authz::postings`, tag 0). The threshold the build passes is
/// `tessera_types::SMALL_TERM_THRESHOLD_DEFAULT`.
const SMALL_TERM_ENTITIES: u64 = 32;

/// The entity span one Roaring container addresses, and what one costs: a descriptor and a key,
/// then an array of `u16` offsets or an 8 KiB bitset, whichever the container's cardinality makes
/// smaller.
const ROARING_CONTAINER_SPAN: u64 = 1 << 16;
const ROARING_CONTAINER_HEADER: u64 = 8;
const ROARING_BITSET_BYTES: u64 = 8 << 10;

/// The three bitmaps one worker of the term-image pass holds while it projects one term, in bytes:
/// the posting over entity space, the image over row space, and the buffer the image is frozen
/// into. The projection scratch is charged beside them.
///
/// ⊘ **Modelled**, and a ceiling: each is charged at a bitset container per 65,536 values, which is
/// a byte per eight and is what a term held by every entity or every row would cost. `n` stands for
/// both spaces, a build's entity ids being `[0, n)` and no view holding more rows than that.
fn term_image_bitmap_bytes(n: u64) -> u64 {
    // Values a byte of bitset container covers: 65,536 over 8 KiB is eight.
    let values_per_byte = ROARING_CONTAINER_SPAN / ROARING_BITSET_BYTES;
    n.div_ceil(values_per_byte).saturating_mul(3)
}

/// What `terms/postings.arrow` comes to for a relation whose terms have these pre-dedup row
/// counts, over an entity space of `n`.
///
/// **Not `4 × pairs`, in either direction.** The file is one `LargeBinaryArray` record a term —
/// `8(T+1)` bytes of offset, then a tag byte and a payload — and a term of cardinality `c ≤ 32`
/// writes `4c` raw bytes. So a relation of `T` singleton terms costs 13 bytes a pair where `4p`
/// charges 4, which is the per-record ACL shape; measured **4.91 and 5.22 B/pair** on two
/// 10⁷-term relations over 10⁸ pairs. Above the threshold the payload is portable Roaring, which
/// is charged here at an array container's two bytes a member, capped at a bitset's 8 KiB, plus a
/// container header per 65,536 of entity space the term can reach.
///
/// ⊘ **The Roaring half is a ceiling and a loose one.** A run container costs four bytes a run
/// whatever it holds, so a term whose entities are contiguous in Morton order encodes far below
/// this: GBIF's 253 country terms over 125,789,091 occurrences are 33,714 bytes, against the
/// hundreds of megabytes charged. Nothing in the declaration says which shape a term will have.
fn postings_bytes(term_rows: &[u64], n: u64) -> u64 {
    let mut total = 8u64.saturating_mul(term_rows.len() as u64 + 1);
    for &rows in term_rows {
        total = total
            .saturating_add(1)
            .saturating_add(term_postings_bytes(rows, n));
    }
    total
}

/// One term's posting record, payload only.
fn term_postings_bytes(rows: u64, n: u64) -> u64 {
    if rows == 0 {
        return 0;
    }
    if rows <= SMALL_TERM_ENTITIES {
        return 4 * rows;
    }
    let containers = n.div_ceil(ROARING_CONTAINER_SPAN).min(rows).max(1);
    let payload = (2 * rows).min(ROARING_BITSET_BYTES.saturating_mul(containers));
    payload.saturating_add(ROARING_CONTAINER_HEADER.saturating_mul(containers))
}

/// How many bytes a LEB128 varint of `value` occupies: seven bits a byte, one byte for zero.
fn varint_len(value: u64) -> u64 {
    let bits = 64 - u64::from(value.leading_zeros());
    bits.div_ceil(7).max(1)
}

/// **What the plan learned about this corpus** that the declaration does not carry: the item
/// count, the access relation's shape and the batching the memory budget derived. Every field is
/// known once pass one and the dictionary have run, which is where the pre-flight sits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Corpus<'a> {
    /// Items, post-dedup — the entity space's size.
    pub n: u64,
    /// The access relation's **pre-dedup** row count.
    pub pair_rows: u64,
    /// Each term's pre-dedup rows, indexed by term id — the postings' own denominator.
    pub term_rows: &'a [u64],
    /// How many signature batches the budget derived.
    pub batches: u64,
    /// Whether the packed pair buckets are held rather than spilled.
    pub bucket_in_ram: bool,
}

/// The corpus's **source ids**, as the two terms that are functions of them need them.
///
/// Both are known before the plan: pass one has read every view's ids into one array and sorted
/// it (`crate::pipeline::read_source_ids_union`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct IdShape {
    /// **The length the ids file is allocated at, and zero where there is no file.**
    ///
    /// Pass one writes no array where it can prove the union is one unbroken range before reading
    /// the ids, which is every corpus that numbers its rows ([`crate::pipeline::SourceIds`]).
    /// Where it does write one and read the ids into it unsorted, the length is the rows every
    /// view declares before the union dedups them — `n` is what survives, and the two differ by
    /// however much the views overlap: 1.76× on `treeoflife-1m`, 4.29× on `multiview`.
    pub slots: u64,
    /// **The largest source id**, which is what sets the varint width the member spill's runs are
    /// charged at. Not the distance between the lowest and the highest: an artifact's first source
    /// is written absolutely and only the rest as deltas (`crate::spill`'s member runs), so a
    /// corpus whose ids are large and narrowly spread pays the full width on every artifact's
    /// first entry. Every value a run encodes is at most this one.
    pub max_id: u64,
}

impl IdShape {
    /// The shape of a corpus whose ids are exactly `0..n` — one view, no duplicates, dense ids.
    #[cfg(test)]
    fn dense(n: u64) -> IdShape {
        IdShape {
            slots: n,
            max_id: n.saturating_sub(1),
        }
    }
}

/// The fixed width one entity's value occupies in [`crate::column::EntityColumn`]'s typed
/// storage. A variable-width type answers [`ARENA_OFFSET`] here — its offset and its record
/// header — and carries its characters in [`ColumnCost::payload_bytes`].
fn fixed_width(ty: ScalarType) -> u64 {
    match ty {
        ScalarType::Bool => 1,
        ScalarType::U8 | ScalarType::I8 => 1,
        ScalarType::U16 | ScalarType::I16 => 2,
        ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 4,
        ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 8,
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => arena_offset(ty),
    }
}

/// What an arena of `payload` characters takes on the disk: the characters and one growth step.
///
/// [`crate::spill::MappedArena`] reserves its blocks rather than leaving the file sparse, so its
/// capacity is disk the build has taken. Past [`crate::spill::ARENA_GROWTH_STEP`] the capacity is
/// within one step of the payload; below it the arena doubles, so a small arena is charged a whole
/// step where it takes at most twice what it holds. That over-charge is a constant per string
/// column and the exactness above it is what the step was added for — the doubling alone cost
/// 3.66 GB of never-written blocks on 125,789,091 GBIF occurrences
/// (`probes/2026-09-10-build-disk/`).
fn arena_capacity(payload: u64) -> u64 {
    payload.saturating_add(crate::spill::ARENA_GROWTH_STEP)
}

/// What one layer member **entry** — one `(artifact, source)` pair — costs the **machine** while
/// the batch holding it is in flight: **12 bytes**, measured.
///
/// A level whose members are scattered across entity space is Roaring array containers almost
/// throughout, and 3.4×10⁹ entries came to 46 GB of anonymous memory over the two copies that
/// stand at once — the bitmaps the publication builds, and the copy `prepare_publish` takes of
/// each (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §5). The figure
/// was 4 until that measurement, which is where a level of that size passed a pre-flight it then
/// exceeded elevenfold.
///
/// **Charged over one batch's entries, not a level's and not the layer's.**
/// [`crate::layers::publish`] publishes a level in batches sized by
/// [`crate::layers::publication_batch_entries`] and encodes each batch into the level's membership
/// pack before the next is built, so what stands is a batch. A level smaller than a batch is
/// charged its own entries.
///
/// ⊘ **The Roaring figure is the scattered case and is not measured per build.** A dense membership
/// costs an eighth of it; the model takes the expensive one, because the refusal it feeds is meant
/// to be wrong in the direction that costs a rerun rather than a kill.
///
/// **It is [`crate::layers::PUBLICATION_BYTES_PER_ENTRY`] and not a second figure beside it.** The
/// batch is cut at that rate, so charging any less here is charging half a batch and calling it the
/// peak — which is what this constant did while it was 12, the one copy rather than the two that
/// stand together. Derived rather than restated so the two cannot drift.
const BYTES_PER_MEMBER_ENTRY: u64 = crate::layers::PUBLICATION_BYTES_PER_ENTRY;

/// What one member entry costs the **disk**, and the process's page cache, as the packed membership
/// extent the store then reads through: 3 bytes, an array container's own width and a little.
///
/// ⊘ **An estimate over the corpora on this box, not a ceiling over every membership.** Measured
/// at 0.98 to 2.40 bytes an entry across seven bundles — 0.980 on PaperSeek's 496,443,271 entries,
/// 1.153 on the 10⁷ MedCPT sample's 471,743,606, 1.508 on GeoNames, 2.377 on `treeoflife-1m`,
/// 2.403 on `gbif-64p`, and 2.236 on GBIF's three taxonomy levels over 125,789,091 occurrences
/// (843.8 MB for 377,367,273 declared key values). Three is charged because it is above every one
/// of them.
///
/// **The shape that exceeds it is many small artifacts whose members are uncorrelated with entity
/// order**: a fine taxonomy over a large corpus, an owner-per-record layer, a layer over an
/// embedding view whose entity order is a k-NN layout rather than the layer's own axis. Measured
/// against portable Roaring over a 4×10⁹ entity space: 2.05 B an entry at 10⁷ members an artifact,
/// **5.93 at 10⁵, 9.98 at 10³ and 14.00 at 4**. Most of that is not the bitmap — it is the 8 bytes
/// of offset every artifact pays and one container header a scattered member — so no per-entry
/// constant covers it, and the denominator that would is the artifact count, which the member
/// file's footer does not carry.
///
/// **Charged in the column window**, which is where these bytes are: the extents are written at the
/// layer publication (stage 8c) and mapped from there to the end of the build, and the declared
/// columns are still on the disk until the release two stages later. They are the bundle's own
/// bytes and would be needed whether or not the store read them back — what the mapping changes is
/// that they are also resident, as page cache the kernel may evict.
const MAPPED_BYTES_PER_MEMBER_ENTRY: u64 = 3;

/// What one layer member entry costs the **disk** while the publication is running: the sorted runs
/// the member spill writes and the merged member table it reads back, both under `.build-tmp/` and
/// both alive at once — the runs are deleted only once the whole table is written.
///
/// Both are LEB128 delta encodings over ascending values, so the true figure falls wherever a
/// membership is dense in its id space and rises where it is scattered.
///
/// **The two encode over different spaces, and only one of them is entity space.** The merged
/// table's deltas are over entities, which live in `0..n`; the runs' are over **source ids**
/// (`crate::spill`'s member runs), which are whatever the corpus's publisher issued. A corpus
/// whose ids are the row number spends one or two bytes on a delta; one whose ids are hashes over
/// the `u64` space spends nine or ten. [`member_spill_bytes`] is where that difference is charged.
///
/// ⊘ **Four is an estimate for a corpus whose ids are dense, not a ceiling over every id space.**
/// GBIF's taxonomy spills at most 1,084.6 MB of runs and table together over 377,367,273 declared
/// key values — 2.9 B an entry — at four row counts from 16.3×10⁶ to 125.8×10⁶ and within 4% of
/// each other (`probes/2026-09-10-build-disk/`); the split is 1.50 for the runs and 1.37 for the
/// table. GeoNames' two member files carry 68.4×10⁶ pairs and spilled 73 MiB of runs beside a
/// 68 MiB table — 2.2 B an entry.
const SPILLED_BYTES_PER_MEMBER_ENTRY: u64 = 4;

/// What the member spill's runs and merged table come to for `entries` pairs over an id space
/// topping out at [`IdShape::max_id`] and an entity space of `n`.
///
/// [`SPILLED_BYTES_PER_MEMBER_ENTRY`] for the pair of files, plus **whatever wider a source-id
/// entry is than an entity one**: the runs encode source ids and the table entities, so a corpus
/// whose ids are hashes pays the extra varint bytes on the runs alone. The difference is zero for
/// every corpus whose ids are its row numbers, which is every corpus on the ladder, and six bytes
/// an entry for ids spread over the whole `u64` space.
fn member_spill_bytes(entries: u64, ids: IdShape, n: u64) -> u64 {
    let sparse = varint_len(ids.max_id).saturating_sub(varint_len(n));
    entries.saturating_mul(SPILLED_BYTES_PER_MEMBER_ENTRY + sparse)
}

/// The residency of everything the batch loop's model does not cover.
///
/// `member_entries` is every `(artifact, source)` pair the layers' member sources declare and
/// `level_entries` a ceiling on any one level's, those being the two windows a member entry is held
/// in: the spill holds every pair at once, the publication one level's. `layer_entries` is the
/// largest single layer's declared pairs, which is what the artifact pass partitions a level at a
/// time. `n` is the item count, and `ids` the source ids' own shape ([`IdShape`]).
///
/// `scoped_render` is the group-scoped render columns of the view that carries the most of them
/// ([`scoped_render_types`]). They are not in `columns`, which is the declaration's entity-scoped
/// attributes, and they open a lane in the assembly exactly as those do.
#[allow(clippy::too_many_arguments)]
pub(crate) fn entity_order_residency(
    n: u64,
    ids: IdShape,
    columns: &[ColumnCost],
    scoped_render: &[ScalarType],
    member_entries: u64,
    level_entries: u64,
    layer_entries: u64,
    memory_budget: u64,
) -> Residency {
    let batch_entries = level_entries.min(crate::layers::publication_batch_entries(memory_budget));
    let publication_bytes = batch_entries.saturating_mul(BYTES_PER_MEMBER_ENTRY);
    let mut terms = vec![
        // **A file under `.build-tmp/` where there is one at all**, and so charged to the disk
        // rather than to memory. The ids are read sequentially by every pass but one — the join's
        // merge sweep, the external-id write, the ordinal walks — and the exception is
        // `layers::publish`'s binary search, which is the random-access case `MappedArray` was
        // written for. They are released at the layer publication, so they are on the disk for
        // every phase of the pre-flight but the assembly.
        //
        // **Charged over the slots the file is allocated at and not over `n`**, and at nothing
        // where pass one proved the union is one unbroken range and wrote no array
        // ([`IdShape::slots`]). Where it did write one, a corpus whose views overlap holds a file
        // larger than the entity space it produces: the union is allocated at the sum of the
        // views' row counts and the dedup moves values inside it.
        Term {
            what: format!(
                "the sorted source ids, 8 B over {} slot(s), in .build-tmp/ (released at the \
                 layer publication)",
                ids.slots
            ),
            bytes: 8u64.saturating_mul(ids.slots),
            mapped: true,
            phases: Phases::SPILL.and(Phases::BANDS).and(Phases::JOIN),
            constant: false,
        },
        // **File-backed since 2026-09-09**, and so charged to the disk rather than to memory: the
        // map is written scattered once and read scattered thereafter, which is `MappedArray`'s
        // own case, and 4 B/item is 13.3 GiB at the GBIF rung. It is page cache the kernel may
        // evict, not memory the machine must have.
        Term {
            what: "the ordinal→entity map, 4 B/item, in .build-tmp/".into(),
            bytes: 4 * n,
            mapped: true,
            // Written by the assignment walk and read to the last view's permutation.
            phases: Phases::BANDS.onwards(),
            constant: false,
        },
    ];
    for (index, column) in columns.iter().enumerate() {
        let width = fixed_width(column.ty);
        let presence = n.div_ceil(8);
        // **A spilled column has nothing in entity order at all** — no arena, no offset array
        // and no presence bitmap ([`crate::column::EntityColumn::spilled`]). What it has instead
        // is one record-blob extent per join chunk, holding the same characters compressed
        // ([`EXTENT_SHARE`]).
        let spilled = column.extents;
        let bytes = if spilled {
            (column.payload_bytes / EXTENT_SHARE)
                .saturating_add(column.framing_bytes / EXTENT_SHARE)
        } else if column.payload_bytes > 0 {
            // 8 bytes of entity-indexed offset and the presence bit, plus the arena the characters
            // and — for a `text` column — their record headers fill.
            8u64.saturating_mul(n)
                .saturating_add(presence)
                .saturating_add(arena_capacity(
                    column.payload_bytes.saturating_add((width - 8) * n),
                ))
        } else {
            width.saturating_mul(n).saturating_add(presence)
        };
        let ty = column.ty.arrow_type_name();
        terms.push(Term {
            what: if spilled {
                format!(
                    "declared column {index} ({ty}): {} MiB of extents in .build-tmp/, \
                     modelled at half the source's {} MiB of characters and their row framing",
                    ((column.payload_bytes + column.framing_bytes) / EXTENT_SHARE) >> 20,
                    column.payload_bytes >> 20
                )
            } else if column.payload_bytes > 0 {
                format!(
                    "declared column {index} ({ty}): {width} B/item of offset plus \
                     {} MiB of characters, in .build-tmp/",
                    column.payload_bytes >> 20
                )
            } else {
                format!("declared column {index} ({ty}): {width} B/item, in .build-tmp/")
            },
            bytes,
            mapped: true,
            phases: column.phases,
            constant: false,
        });
        // **The join's `(entity, at)` partition**, for a string column whose characters fill an
        // arena (`docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §4.3). The arena is
        // appended in arrival order and the word that says where a value went is indexed by
        // entity, so the word goes to a partition by entity range and each bucket is written into
        // the offset array as one run — 12 B an item, standing from the join's first chunk to the
        // replay and released bucket by bucket there. A spilled column has no offset array and
        // pushes nothing.
        if !spilled && carries_characters(column.ty) {
            terms.push(Term {
                what: format!(
                    "declared column {index} ({ty}): the join's (entity, at) partition, 12 B/item, \
                     in .build-tmp/"
                ),
                bytes: 12u64.saturating_mul(n),
                mapped: true,
                phases: Phases::JOIN,
                constant: false,
            });
        }
        // **A spilled column's duplicate map** (`crate::extents::DuplicateMap`): the two
        // whole-column Roaring bitmaps that stand while the map says which of a repeated entity's
        // rows survives. It is built when the extents are opened and held until the column's last
        // reader is done with them, and it is the one anonymous term a spilled column has — the
        // rest of what an open extent costs is a block buffer, which the merge fan-in bounds.
        //
        // **It stands for exactly the phases the column's own storage does, less the join**, which
        // is what `column.phases` says: a spilled column with no blob row meets its last reader at
        // the end of the index phase and `crate::pipeline` drops its `OpenExtents` there, so
        // charging it through the blob would refuse a build over bytes nothing holds. The map does
        // not exist in the join at all — the extents are still being written then.
        //
        // **A rate over the corpus, not a constant**, which is what separates it from the
        // partitions below: a Roaring bitmap over an entity space of n is at most n/8 bytes, and a
        // spilled column's extents between them cover the entities that have a value. Charging the
        // ceiling over-charges a column whose extents are sparse or run-encoded, and the ceiling is
        // the figure the budget has to stand: 437 MB apiece at the 3.5×10⁹-row GBIF rung. The eight
        // bytes a *repeated* entity that the built table costs are not charged — a corpus with one
        // row an entity repeats none, and one that repeated every entity would pay 8n against these
        // 2n/8, which is a shape the model does not have a figure for.
        if spilled {
            terms.push(Term {
                what: format!(
                    "declared column {index} ({ty}): the duplicate map over its extents, two \
                     whole-column Roaring bitmaps at {} MiB apiece",
                    n.div_ceil(8) >> 20
                ),
                bytes: 2u64.saturating_mul(n.div_ceil(8)),
                mapped: false,
                phases: match column.phases.holds(Phase::Blob) {
                    true => Phases::INDEX.and(Phases::BLOB),
                    false => Phases::INDEX,
                },
                constant: false,
            });
        }
        // The text index's sorted runs, **charged at the column they are tokenised from** rather
        // than at a constant of their own. The runs spill while the column is resident, so the two
        // stand on the disk together; and a run spends one varint on a `(term, entity)` pair where
        // the prose spent the term's whole characters on every occurrence of it, so the column is
        // a ceiling over them. Charging them the column's arithmetic rather than a second copy of
        // it is also what keeps the two from drifting apart.
        //
        // ⊘ The ceiling is modelled, not measured, and it is loose: the one corpus with a run
        // figure — 7.4×10⁷ Overture names — spilled 555 MB against the 2.58 GB this charges. The
        // slack is the payload figure's own, being Parquet's encoded page size for the column
        // rather than its characters.
        if column.text_index {
            terms.push(Term {
                what: format!(
                    "the text index's sorted runs over column {index}, charged at the column they \
                     are tokenised from"
                ),
                // The column's own storage. For a spilled column the runs are charged at the
                // source's characters rather than at the extents that hold them, the runs being
                // uncompressed.
                bytes: if spilled { column.payload_bytes } else { bytes },
                mapped: true,
                phases: Phases::INDEX,
                constant: false,
            });
        }
    }
    // **And the duplicate maps' transient, charged once rather than per column.** Building one
    // column's map holds four whole-column bitmaps at its worst moment — the running union, the
    // repeats so far, the extent's own bitmap and the intersection of the first with the third —
    // against the two the built map leaves standing (`crate::extents::DuplicateMap`). The columns
    // are opened one after another, so only one column is ever mid-build: the extra two bitmaps are
    // a term of the build and not of the column count, and charging them per column would
    // over-charge the second spilled column by n/4 for bytes that are never simultaneously live.
    if columns.iter().any(|column| column.extents) {
        terms.push(Term {
            what: format!(
                "the two further whole-column Roaring bitmaps held while one spilled column's \
                 duplicate map is built, at {} MiB apiece",
                n.div_ceil(8) >> 20
            ),
            bytes: 2u64.saturating_mul(n.div_ceil(8)),
            mapped: false,
            phases: Phases::INDEX,
            constant: false,
        });
    }
    if member_entries > 0 {
        terms.push(Term {
            what: format!(
                "{batch_entries} member entr(ies) in one publication batch at \
                 {BYTES_PER_MEMBER_ENTRY} B — the publication's own Roaring, while the batch it \
                 is publishing is in flight (the largest level holds {level_entries})"
            ),
            bytes: publication_bytes,
            mapped: false,
            phases: Phases::JOIN,
            constant: false,
        });
        terms.push(Term {
            what: format!(
                "the published memberships the store reads back through the packed extent, at \
                 an estimate of {MAPPED_BYTES_PER_MEMBER_ENTRY} B a member entry"
            ),
            bytes: member_entries.saturating_mul(MAPPED_BYTES_PER_MEMBER_ENTRY),
            mapped: true,
            // The bundle's own extents: written at the publication and never released.
            phases: Phases::JOIN.onwards(),
            constant: false,
        });
        terms.push(Term {
            what: format!(
                "the member spill's runs and the table they merge into, at an estimate of \
                 {} B a member entry, in .build-tmp/",
                member_spill_bytes(1, ids, n)
            ),
            bytes: member_spill_bytes(member_entries, ids, n),
            mapped: true,
            phases: Phases::JOIN,
            constant: false,
        });
    }
    // **The partitions, charged at the key type's ceiling and not at the corpus.** A bucket holds
    // at most [`crate::spill::PARTITION_BUCKET_RECORDS`] records however many rows the build has,
    // so what a partition costs is a constant: its writer buffers while it is open, and one loaded
    // bucket and one window at the replay. Charging the ceiling over-charges a small corpus by up
    // to the bucket count, which is the price of a model whose terms do not move with `n` — the
    // whole of what `the_anonymous_total_grows_only_by_the_four_terms_this_names` asserts, and
    // the reason the design names 2 GiB as the budget floor (§3).
    //
    // **How many are open at the worst phase.** The attribute join opens one per column it fills
    // in entity order and holds them all while it sweeps; the assembly opens the row partition,
    // then the pairs beside it, then every render lane together — one pass over the pairs feeds
    // them all (`crate::assembly::RenderLane`). Only one bucket is loaded at any moment in either,
    // the replays being sequential.
    //
    // **A string column on the arena route has one too**, carrying the eight-byte `at` word where a
    // fixed-width column carries its value (§4.3). A spilled column has no lane at all: its
    // characters go straight to record-blob extents.
    let join_partitions = columns.iter().filter(|column| !column.extents).count() as u64;
    let join_width = columns
        .iter()
        .filter(|column| !column.extents)
        .map(|column| match carries_characters(column.ty) {
            true => 4 + 8,
            false => 4 + fixed_width(column.ty),
        })
        .max()
        .unwrap_or(0);
    // The same arithmetic the partition itself sizes its writers by, so what the pre-flight
    // charges is what the pass allocates.
    let buffers = |buckets: u64, width: u64| {
        buckets
            .saturating_mul(crate::spill::partition_buffer_bytes(n, width as usize, buckets) as u64)
    };
    // **The corpus where it is smaller than the type's bound, the bound where it is not.** A
    // bucket holds at most `n / 128` records and never more than 2³²/128 whatever `n` is, so the
    // term rises with the corpus until the key type binds and is flat above it — which is the
    // sense in which a partition's memory is a constant, and what
    // `the_anonymous_total_grows_only_by_the_four_terms_this_names` asserts by evaluating the
    // model on both sides of that bound.
    let records = (n / crate::spill::PARTITION_BUCKETS as u64)
        .clamp(1, crate::spill::PARTITION_BUCKET_RECORDS);
    let bucket = |width: u64| records.saturating_mul(width);
    if join_partitions > 0 {
        terms.push(Term {
            what: format!(
                "the attribute join's {join_partitions} value partition(s): {} MiB of writer \
                 buffers each, and one loaded bucket and window at the widest column's {join_width} \
                 B a record over the {records} records a bucket holds",
                buffers(crate::spill::PARTITION_BUCKETS as u64, join_width) >> 20
            ),
            bytes: buffers(crate::spill::PARTITION_BUCKETS as u64, join_width)
                .saturating_mul(join_partitions)
                .saturating_add(bucket(join_width))
                .saturating_add(bucket(join_width.saturating_sub(4))),
            mapped: false,
            phases: Phases::JOIN,
            constant: true,
        });
    }
    // The keyword dictionary's `(row, ordinal)` partition, where a column is indexed at all.
    if columns
        .iter()
        .any(|column| matches!(column.ty, ScalarType::Keyword))
    {
        terms.push(Term {
            what: "the keyword dictionary's (row, ordinal) partition: writer buffers, one loaded \
                   bucket at 8 B a record and the u32 window it is scattered into"
                .into(),
            bytes: buffers(crate::spill::PARTITION_BUCKETS as u64, 8)
                .saturating_add(bucket(8))
                .saturating_add(bucket(4)),
            mapped: false,
            phases: Phases::INDEX,
            constant: true,
        });
    }
    // **The assembly** (`crate::assembly`): the Morton histogram, which is the `morton >> 8` space
    // and so a constant of the code type; the row partition's counted buckets; and one loaded
    // bucket sorted into 24 B records beside the 12 B it was read as. The 24 is
    // `crate::assembly::RowRec`'s own size, which its `u64` alignment fixes there whatever order
    // the fields are declared in.
    terms.push(Term {
        what: format!(
            "the segment assembly: a {} MiB Morton histogram, the row \
             partition's {} MiB of writer buffers, and one bucket of {records} records held as \
             the 12 B it was written at and the 24 B it is sorted as",
            crate::assembly::MortonHistogram::bytes_for_rows(n) >> 20,
            buffers(crate::spill::PARTITION_COUNTED_BUCKETS, 12) >> 20
        ),
        bytes: crate::assembly::MortonHistogram::bytes_for_rows(n)
            .saturating_add(buffers(crate::spill::PARTITION_COUNTED_BUCKETS, 12))
            .saturating_add(bucket(12))
            .saturating_add(bucket(24)),
        mapped: false,
        phases: Phases::ASSEMBLE,
        constant: true,
    });
    // The widest render column is a ceiling over the fixed-width ones: `render` is refused at the
    // declaration for every string type, so a render column is one of these and no wider. A
    // group-scoped family is not in `columns` — its columns are read per view and never routed
    // ([`crate::pipeline::may_take_extents`]) — so its types are measured beside them.
    let widest_render = join_width.saturating_sub(4).max(
        scoped_render
            .iter()
            .map(|&ty| fixed_width(ty))
            .max()
            .unwrap_or(0),
    );
    //
    // **Every render lane's writer buffers stand together.** The `(entity, row)` bucket is loaded
    // and sorted once and handed to the permutation and to every lane from that one sweep, rather
    // than being loaded and sorted again per column, so each lane's buffers are open while the
    // pairs are consumed. Counted at the columns the declaration renders; their width is
    // [`widest_render`]'s ceiling, `render` being refused at the declaration for every string
    // type. A view with no render column still pays one lane's worth here, which is the floor a
    // build under a tight budget is charged rather than a lane it opens.
    //
    // **A group-scoped render family opens a lane of its own** in the row space of every view its
    // scope reaches (`views.md` §5), so `scoped_render` carries the types of the view that opens
    // the most. One view's row space is assembled at a time, and that view is the peak.
    let render_lanes =
        (columns.iter().filter(|column| column.render).count() + scoped_render.len()).max(1) as u64;
    terms.push(Term {
        what: format!(
            "the assembly's (entity, row) and {render_lanes} (row, value) partition(s): writer \
             buffers, the loaded pairs bucket at 8 B a record, and one lane's bucket at up to {} B \
             a record, the window it is placed in and a byte a row saying which rows it filled",
            4 + widest_render
        ),
        bytes: buffers(crate::spill::PARTITION_BUCKETS as u64, 8)
            .saturating_add(
                buffers(crate::spill::PARTITION_BUCKETS as u64, 4 + widest_render)
                    .saturating_mul(render_lanes),
            )
            .saturating_add(bucket(8))
            .saturating_add(bucket(4 + widest_render))
            .saturating_add(bucket(widest_render))
            // The lane replay's `filled` flags: a byte a row of the bucket's range, alive while
            // the window is, and what the presence bitmap beside the column is derived from.
            .saturating_add(bucket(1)),
        mapped: false,
        phases: Phases::ASSEMBLE,
        constant: true,
    });
    // **The artifact pass's `(row, ordinal)` partition** (the design memo §4.6). A level's column
    // was composed into a row-sized lane — 4 B a row, 14 GB a level at rung 6 — and is now composed
    // through a partition by row range, one level at a time: the writer buffers, one loaded bucket
    // at 8 B a record, the 8 B keys that bucket is sorted into, and for the list form a `u32`
    // window counting the bucket's rows.
    //
    // **A record is a member entry and not a row**, so this bucket is not bounded by the key type
    // the way the other partitions' are. A row belongs to one artifact of a single-valued level and
    // to many of a list-form one — MedCPT declares 47 entries a row — so the bucket holds the
    // larger of the row space and the largest layer's entries, over the bucket count. The window is
    // the row one: it counts the rows of the bucket's range and nothing else.
    let pass_records = records.max(layer_entries / crate::spill::PARTITION_BUCKETS as u64);
    terms.push(Term {
        what: format!(
            "the artifact pass's (row, ordinal) partition: {} MiB of writer buffers, one loaded \
             bucket of {pass_records} member entries at 8 B each and the 8 B keys it is sorted \
             into, and the u32 window the list form counts its rows in",
            buffers(crate::spill::PARTITION_BUCKETS as u64, 8) >> 20
        ),
        bytes: buffers(crate::spill::PARTITION_BUCKETS as u64, 8)
            .saturating_add(pass_records.saturating_mul(8))
            .saturating_add(pass_records.saturating_mul(8))
            .saturating_add(bucket(4)),
        mapped: false,
        phases: Phases::ASSEMBLE,
        constant: true,
    });
    // **The term images** (`crate::term_images_pass`, pipeline step 10c). The pass holds one term
    // per worker in flight: that term's posting as an owned bitmap over entity space, its image
    // over row space, the buffer the image is serialised into, and one projection scratch. A
    // Roaring container covers 65 536 values and costs at most 8 KiB, at which point it is a bitset
    // over every value in its range, so a term held by every entity is the widest posting
    // expressible and one held by every row the widest image. The frozen buffer is charged at the
    // image's width: it is the containers' payloads with five bytes of key, count and typecode
    // each, which is under the resident form for every shape but an image of full bitsets. The
    // scratch is `tessera_store`'s own bound over this corpus's entity space, so a small build pays
    // its own rows rather than the ceiling a 10⁹ one reaches. Nothing in the pass scales with the
    // dictionary: the table is written into the file as the pass's row buffer fills, and that
    // buffer is a constant 160 KiB.
    //
    // **The worker count is the pass's own** (`crate::term_images_pass::derive_threads`), which is
    // capped rather than the machine's width, and capped for this term: three bitmaps a worker is
    // about 1.3 GB at rung 6, so an uncapped width forecasts past the budget such a build runs
    // under.
    //
    // ⊘ **Modelled**, and a ceiling rather than an expectation: a term over a third of the corpus
    // in run-friendly order is kilobytes (assumed). The fold's memory estimate charges the same
    // window over its own one thread, at a flat ceiling for the scratch rather than this bound.
    let image_workers = crate::term_images_pass::derive_threads() as u64;
    let scratch = tessera_store::permutation::project_scratch_bound(n, n);
    terms.push(Term {
        what: format!(
            "the term images, over {image_workers} worker(s): one term's posting, its image and \
             the buffer it is frozen into, each at a bitset container per 65,536 values"
        ),
        bytes: image_workers.saturating_mul(term_image_bitmap_bytes(n)),
        mapped: false,
        phases: Phases::ASSEMBLE,
        constant: false,
    });
    terms.push(Term {
        what: format!(
            "the term images' projection scratch, {} MiB over {image_workers} worker(s)",
            scratch.total() >> 20
        ),
        bytes: image_workers.saturating_mul(scratch.total()),
        mapped: false,
        phases: Phases::ASSEMBLE,
        // The pool never exceeds one window of row ids and a chunk a bucket, and the stamp and its
        // marks are the same size whatever the corpus. The term rises to that bound and is flat
        // above it, which is the shape the partitions' buckets have.
        constant: true,
    });
    // **Arrow's all-ones validity bitmaps, during the `columns.arrow` layout pass.** Every column
    // of the file is non-nullable and arrow writes a validity buffer for it all the same
    // (`docs/evidence/memos/2026-09-11-arrow-all-ones-validity-buffers.md`). The file keeps them as
    // three numbers (`tessera_store::columns::Fill`) but the layout pass does not: arrow's IPC
    // writer builds every buffer of the batch before it writes any of them, so one `n / 8`-byte
    // bitmap per column is alive at once — 437 MB a column at rung 6, anonymous, and a rate over
    // `n` rather than a constant.
    //
    // **Two fixed columns plus the render columns**, counted here as every fixed-width declared
    // column, which is the same ceiling [`widest_render`] takes: `render` is refused at the
    // declaration for every string type, so a render column is one of these. A build that declares
    // fixed-width columns it does not render is over-charged by `n / 8` each.
    let render_columns = columns
        .iter()
        .filter(|column| !carries_characters(column.ty))
        .count() as u64;
    terms.push(Term {
        what: format!(
            "arrow's all-ones validity bitmaps over {} column(s) at n/8 bytes each, alive together              while the columns.arrow layout is learnt",
            2 + render_columns
        ),
        bytes: (2 + render_columns).saturating_mul(n.div_ceil(8)),
        mapped: false,
        phases: Phases::ASSEMBLE,
        constant: false,
    });
    terms.push(Term {
        what: "slack for decode buffers, stage scratch and the allocator".into(),
        bytes: SLACK,
        mapped: false,
        phases: Phases::JOIN,
        constant: true,
    });
    Residency { terms }
}

/// What the entity-order model is arithmetic over for this build: one [`ColumnCost`] per declared
/// column, every member entry the layers declare, and a ceiling on any one level's.
///
/// **Footers, and about 2×10⁶ values a string column.** The row counts are metadata; the characters
/// are not, and the metadata figure for them is wrong by a factor of four on the corpus that ran
/// out of disk ([`ColumnCost`]). A file that cannot be opened, or a column that is not in it,
/// contributes zero rather than refusing: the pre-flight is an estimate, and a build blocked
/// because a footer would not parse is a worse outcome than one that under-reads.
///
/// Separated from [`entity_order_residency`] because [`plan_routes`] varies one column's route at
/// a time and must not re-read a footer per candidate.
fn model_inputs(
    args: &crate::BuildArgs,
    n: u64,
    payloads: &[f64],
    routes: &crate::pipeline::ColumnRoutes,
) -> (Vec<ColumnCost>, u64, u64, u64) {
    // Scaled by `n` rather than taken whole, so a `--limit` build is charged the prefix it builds
    // and not the file it reads from.
    let columns: Vec<ColumnCost> = args
        .schema
        .attributes
        .iter()
        .zip(payloads)
        .enumerate()
        .map(|(index, (attribute, &per_item))| ColumnCost {
            ty: attribute.ty,
            payload_bytes: payload(per_item, n),
            // The build's own routing, read rather than restated: a model that decided the route
            // for itself would charge an arena the build no longer fills, or the other way round
            // on the next column whose readers change.
            extents: routes.takes_extents(index),
            // The frame around each extent row, which is the route's and not the type's
            // ([`extent_framing_bytes`]).
            framing_bytes: match routes.takes_extents(index) {
                true => extent_framing_bytes(n),
                false => 0,
            },
            // The same test the emit itself makes, called rather than restated: a text column
            // earns an index exactly where it is owed postings.
            text_index: attribute.ty == ScalarType::Text
                && crate::pipeline::postings_are_owed(&args.schema, attribute),
            render: attribute.render,
            phases: if attribute.render {
                Phases::JOIN.onwards()
            } else if crate::pipeline::blob_resident(&args.schema, attribute) {
                Phases::JOIN.and(Phases::INDEX).and(Phases::BLOB)
            } else {
                Phases::JOIN.and(Phases::INDEX)
            },
        })
        .collect();
    // **Three denominators over the same member sources**, because a member entry is held in
    // windows of three sizes. Every entry is on the disk at once, as the spill's runs and the table
    // they merge into: that is the file's key values. One *level's* are in memory at once, as the
    // publication's Roaring, and a level draws at most one artifact per member row: that is the
    // file's rows, which is a ceiling on any one level whatever the shape of its key lists. One
    // *layer's* are what the artifact pass partitions, a level at a time, and there a row carries
    // one record per artifact it belongs to rather than one in all: that is the layer's declared
    // pairs, and the largest layer is the ceiling over the pass.
    let mut entries = 0u64;
    let mut level_entries = 0u64;
    let mut layer_entries = 0u64;
    for layer in &args.layer_inputs {
        let Some(members) = layer.members.as_ref() else {
            continue;
        };
        let (declared, rows) = member_entries(members);
        entries = entries.saturating_add(declared);
        level_entries = level_entries.saturating_add(rows);
        layer_entries = layer_entries.max(declared);
    }
    (columns, entries, level_entries, layer_entries)
}

/// **How much of the free space an arena has to fit inside to be worth filling.**
///
/// The two routes are not equally wrong. Taking the extents where the arena would have fitted
/// costs a compression pass the build did not need, measured at 5–10% of the whole build's wall
/// clock at four row counts from 16.3×10⁶ to 125.8×10⁶ GBIF occurrences. Taking the arena where
/// it does not fit costs the ENOSPC that stopped the 3.50×10⁹-row rung with 93 GB of its bundle
/// written (`probes/2026-09-10-build-disk/`). One is a percentage and the other is a build
/// that does not finish, so the rule leaves room rather than aiming at the crossover.
///
/// Room for a factor of two, because the output filesystem is shared with whatever else the
/// machine is doing and this choice is made once, at the plan, for the length of the build.
///
/// What is compared against it is [`stage_scratch`], which is the entity-order stages' own files
/// and not the whole build's disk — and on the corpus this was measured over the two are nearly
/// the same number: 2,041 MiB modelled against a 1,853 MiB measured peak at 16.3×10⁶ GBIF
/// occurrences, and 12,519 against 12,011 at 125.8×10⁶, the measured figure being the whole bundle
/// root (`probes/2026-09-10-blob-resident-strings/`). So a build the rule sends down the arena is
/// one the pre-flight then passes rather than warns about, with the second factor of two left over
/// for the corpus whose bundle is a larger share of its peak than GBIF's.
const ROUTE_HEADROOM: u64 = 2;

/// **The route each declared column's characters take, and the residency it settles on.**
///
/// A column with two routes ([`crate::pipeline::may_take_extents`]) takes the arena where the
/// entity-order stages have the disk for it and the record blob's extents where they do not.
/// `free` is the space on the output filesystem, or `None` where that is unknowable, which spills
/// every column the route is available to: the arena is the larger footprint and the one a build
/// runs out of disk on, so an unreadable filesystem takes the smaller.
///
/// **The disk and not the memory budget.** An arena is read at an entity by exactly two passes —
/// the value column's dictionary and the row tail — and a column routed here has neither, so its
/// only reader is the record blob's merge, which walks entity space ascending. The join writes
/// each chunk of the arena in that chunk's entity order, so the merge reads it as a handful of
/// ascending runs rather than at random, and squeezing the page cache does not break it: measured
/// at 125,789,091 GBIF occurrences uncapped and under cgroup caps of 8 and 6 GiB, `record_blob`
/// held at 28.7–28.9 s on the arena route while the extent route's stayed at 39.3–44.5 s
/// (`probes/2026-09-10-blob-resident-strings/`). Below 6 GiB the build is OOM-killed in a stage no
/// route reaches. What the arena does cost is space — 26 to 31% more peak disk at every row count
/// measured — and space is what the 3.50×10⁹-row rung ran out of.
///
/// What is compared is not the arena on its own: it stands beside every other declared column, the
/// sorted source ids, the ordinal→entity map and the member spill, all of which are files in the
/// same window. So the test is the **largest phase of the entity-order stages' own scratch**
/// against the free space with [`ROUTE_HEADROOM`] left over. ⊘ That window is not the whole build's
/// disk: the bundle's own bytes and the postings are [`disk`]'s and no route moves them. The
/// pre-flight covers those and reports on them.
///
/// **One column at a time, in declaration order.** Two arenas that each fit alone need not fit
/// together, so a column is promoted only against the window the promotions before it already
/// bought. Declaration order rather than ascending size: the payload figure is a sample of three
/// row groups ([`payloads_per_item`]), so ordering by it would let a byte of sampling noise
/// re-route two columns of similar size, where the declaration does not move.
///
/// The routes change no byte of the bundle. What they change is where the characters stand while
/// the build runs, which is why this is the disk's business and not the schema's.
pub(crate) fn plan_routes(
    args: &crate::BuildArgs,
    n: u64,
    ids: IdShape,
    payloads: &[f64],
    free: Option<u64>,
) -> (crate::pipeline::ColumnRoutes, Residency) {
    let spilled = crate::pipeline::ColumnRoutes::every_available(&args.schema);
    let (columns, entries, level_entries, layer_entries) =
        model_inputs(args, n, payloads, &spilled);
    choose_routes(
        &args.schema,
        n,
        ids,
        columns,
        &scoped_render_types(args),
        entries,
        level_entries,
        layer_entries,
        free,
        args.memory_budget
            .unwrap_or_else(crate::pipeline::detect_memory_budget),
    )
}

/// [`plan_routes`] unless the caller named the route ([`crate::ExtentRoute`]), in which case the
/// named one is taken and the model is rebuilt over it — so the disk forecast a forced build
/// prints describes the build it is about to run.
pub(crate) fn routes_for(
    args: &crate::BuildArgs,
    n: u64,
    ids: IdShape,
    payloads: &[f64],
    free: Option<u64>,
    route: crate::ExtentRoute,
) -> (crate::pipeline::ColumnRoutes, Residency) {
    let forced = match route {
        crate::ExtentRoute::Derived => return plan_routes(args, n, ids, payloads, free),
        crate::ExtentRoute::Arena => crate::pipeline::ColumnRoutes::forced_only(&args.schema),
        crate::ExtentRoute::Extents => crate::pipeline::ColumnRoutes::every_available(&args.schema),
    };
    let (columns, entries, level_entries, layer_entries) = model_inputs(args, n, payloads, &forced);
    let budget = args
        .memory_budget
        .unwrap_or_else(crate::pipeline::detect_memory_budget);
    let tail = entity_order_residency(
        n,
        ids,
        &columns,
        &scoped_render_types(args),
        entries,
        level_entries,
        layer_entries,
        budget,
    );
    (forced, tail)
}

/// The **group-scoped render columns one view's row space carries**, at the view that carries the
/// most of them (`views.md` §5).
///
/// A scoped family opens a `(row, value)` lane in the assembly exactly as a declared render column
/// does, and it is not in [`ColumnCost`]: its columns are read per view, never routed, and never
/// join a declared column's partition. One view's row space is assembled at a time, so the view
/// with the widest set of lanes is the phase's peak.
///
/// **Which views a family reaches is `pipeline::scoped_render_targets`' rule**: a view of the
/// family's own group, and a view of a group declaring `members` of it. Stated here over
/// `BuildArgs` rather than shared, because that function is over the columns a pass has already
/// read and this runs before any of them exist. The two disagreeing costs a forecast, not a
/// bundle.
fn scoped_render_types(args: &crate::BuildArgs) -> Vec<ScalarType> {
    args.views
        .iter()
        .map(|view| {
            let Some((group, _)) = view.view_id.split_once(tessera_store::GROUP_SEPARATOR) else {
                // A plain view is in no group, so no scope reaches it.
                return Vec::new();
            };
            let owner = args
                .groups
                .iter()
                .find(|descriptor| descriptor.name == group)
                .and_then(|descriptor| descriptor.members_of.as_deref())
                .unwrap_or(group);
            args.scoped_attributes
                .iter()
                .filter(|family| family.group == owner && family.attribute.render)
                .map(|family| family.attribute.ty)
                .collect::<Vec<ScalarType>>()
        })
        .max_by_key(|types| types.iter().map(|&ty| 4 + fixed_width(ty)).sum::<u64>())
        .unwrap_or_default()
}

/// [`plan_routes`] over the model's inputs rather than the build's, so the rule can be tested at a
/// schema and a free-space figure without a corpus behind them. `columns` arrives with every
/// available column spilled.
#[allow(clippy::too_many_arguments)]
fn choose_routes(
    schema: &crate::config::Schema,
    n: u64,
    ids: IdShape,
    mut columns: Vec<ColumnCost>,
    scoped_render: &[ScalarType],
    entries: u64,
    level_entries: u64,
    layer_entries: u64,
    free: Option<u64>,
    memory_budget: u64,
) -> (crate::pipeline::ColumnRoutes, Residency) {
    let mut routes = crate::pipeline::ColumnRoutes::every_available(schema);
    let ceiling = free.unwrap_or(0) / ROUTE_HEADROOM;
    for (index, attribute) in schema.attributes.iter().enumerate() {
        if !routes.takes_extents(index) || crate::pipeline::extents_are_forced(schema, attribute) {
            continue;
        }
        columns[index].extents = false;
        columns[index].framing_bytes = 0;
        let candidate = entity_order_residency(
            n,
            ids,
            &columns,
            scoped_render,
            entries,
            level_entries,
            layer_entries,
            memory_budget,
        );
        // **The arena route carries its offset lane's 12 B an item** — the `(entity, at)` records
        // of the value partition the join replays the `at` words from (`crate::pipeline`'s
        // `ValueLane`) — and that is disk in the same window as the arena itself. A string column
        // whose arena sits just under the ceiling is answered here with the extent route because
        // of it.
        if stage_scratch(&candidate) <= ceiling {
            routes.take_arena(index);
        } else {
            columns[index].extents = true;
            columns[index].framing_bytes = extent_framing_bytes(n);
        }
    }
    let tail = entity_order_residency(
        n,
        ids,
        &columns,
        scoped_render,
        entries,
        level_entries,
        layer_entries,
        memory_budget,
    );
    (routes, tail)
}

/// **What the entity-order stages have on the disk at once**: the largest phase's mapped terms.
///
/// The anonymous terms are not in it. They are the publication's Roaring and one slack constant,
/// neither of which a route moves, and they are memory rather than space.
pub(crate) fn stage_scratch(residency: &Residency) -> u64 {
    residency.peak().1
}

/// The supplied keys' arena, as a term of the entity-order residency (`crate::ids`).
///
/// **Anonymous memory with no spill route**, so it is charged rather than reported: the keys are
/// interned before pass one and read by every pass after it. A key costs its `Box<[u8]>` in the
/// interned vector, 16 bytes, plus its own heap allocation, which glibc rounds to a 16-byte chunk
/// with an 8-byte header and a 32-byte floor. Modelled from the mean key length, so a corpus of
/// widely varying key lengths is charged its mean rather than its distribution.
pub(crate) fn supplied_key_arena(id_space: &crate::ids::IdSpace) -> Option<Term> {
    let keys = id_space.supplied()?;
    let mean = keys.mean_key_len();
    let chunk = (mean + 8).next_multiple_of(16).max(32);
    Some(Term {
        what: format!(
            "the supplied identity keys, at {} B/item: a 16 B boxed slice and a {chunk} B \
             allocator chunk over a {mean} B mean key. Interned before pass one and read by every \
             pass after it, with no spill route",
            16 + chunk
        ),
        bytes: (16 + chunk) * keys.len() as u64,
        mapped: false,
        phases: Phases::SPILL.onwards(),
        constant: false,
    })
}

/// **What the whole build asks the disk for**, phase by phase, so the pre-flight warns on the
/// largest window rather than on a total nothing ever holds.
///
/// The entity-order terms are `tail`'s, carried rather than recomputed — a second copy of that
/// arithmetic is how the two stop agreeing. What is added here is everything outside that window:
/// the pair spills, the geometry, the join's staging buffer, the keyword dictionary's scratch, the
/// row spaces' own files, and **the bundle**, which nothing releases.
///
/// ⊘ **The bundle was not modelled at all until 2026-09-10**, beyond one `4 B/pair` term for the
/// postings. It is 68.5 bytes an item on the GBIF schema when it is finished and 22.5 by the index
/// phase (measured over 16.3×10⁶ to 125.8×10⁶ occurrences, agreeing to within 1%), and the build
/// that ran out of disk at rung 6 had 93 GB of it written when it died
/// (`probes/2026-09-10-build-disk/`).
///
/// ⊘ **Two of the bundle's families still carry no term, both for the same reason**: their
/// denominator is a level's **artifact count**, which nothing knows before the level is published
/// — a layer whose `value_set` is open has no roster to read one from. They are the tile index
/// (`tile-index/*.tsti`, two `u32` an artifact) and the artifact record extents
/// (`attrs/record/extents/`). Measured at 1.49 MB over 25,846,007 items on `gbif-64p` and 435 KB
/// over 10⁷ on the MedCPT sample — 0.06 and 0.04 bytes an item, three orders below the terms
/// below.
pub(crate) fn disk(
    args: &crate::BuildArgs,
    corpus: Corpus<'_>,
    payloads: &[f64],
    tail: &Residency,
    id_space: &crate::ids::IdSpace,
) -> Residency {
    let Corpus {
        n,
        pair_rows,
        term_rows,
        batches,
        bucket_in_ram,
    } = corpus;
    let p = pair_rows;
    let views = args.views.len().max(1) as u64;
    let mut terms: Vec<Term> = tail.terms.iter().filter(|t| t.mapped).cloned().collect();
    let mut push = |what: String, bytes: u64, phases: Phases| {
        if bytes > 0 {
            terms.push(Term {
                what,
                bytes,
                mapped: true,
                phases,
                constant: false,
            });
        }
    };

    // ---- the pair relation's own spills ------------------------------------------------------
    // Stated ceilings for the varint codec and the Roaring postings, not measurements of this
    // corpus: a relation over few terms encodes far below them. A term that errs high costs a
    // rerun; one that errs low is the ENOSPC this exists to pre-empt.
    if !bucket_in_ram {
        push(
            "the packed pair buckets, 8 B/pair, in .build-tmp/ (deleted as the batch loop loads \
             each)"
                .into(),
            8 * p,
            Phases::SPILL,
        );
    }
    push(
        "the first term band beside the buckets, at a ceiling of 6 B/pair".into(),
        (6 * p) / batches.max(1),
        Phases::SPILL,
    );
    push(
        "the term bands, at a ceiling of 6 B/pair, in .build-tmp/".into(),
        6 * p,
        Phases::BANDS,
    );
    // **File-backed since 2026-09-10**, and a disk term for the first time because it never had a
    // memory one here: the label-agreement tally is charged in `pipeline::plan_build`'s
    // `loop_fixed`, which still carries its 4 B/item deliberately so `auto_batch` and the entity
    // ids under it do not move (I9). It is written scattered by the pairs pack and read in ordinal
    // order by the batch loop's label-agreement pass, so it stands from the pairs pass to that
    // loop and no longer — the anchor geometry's window exactly.
    push(
        "the label-agreement tally by ordinal, 4 B/item, in .build-tmp/ (released at the \
         assignment)"
            .into(),
        4 * n,
        Phases::SPILL.and(Phases::BANDS),
    );

    // ---- the geometry --------------------------------------------------------------------
    // Read in ordinal space once per view and released at that view's permutation, so every
    // view's is on the disk together from the geometry pass to the first row space.
    push(
        format!("each view's geometry by ordinal, 8 B/item over {views} view(s), in .build-tmp/"),
        8 * n * views,
        Phases::SPILL.onwards(),
    );
    // **Only where the fallback can reach an item.** A single-view build's anchor holds every
    // ordinal, and `pipeline::build` then reads that view's own arrays rather than writing a copy
    // of them. Charged on a multi-view declaration because whether the anchor covers the union is
    // not known until pass one has run.
    if views > 1 {
        push(
            "the anchor view's Morton geometry, 8 B/item, in .build-tmp/ (released at the \
             assignment)"
                .into(),
            8 * n,
            Phases::SPILL.and(Phases::BANDS),
        );
    }
    // **The assembly's partitions** (`crate::assembly`), where the entity-order geometry and the
    // row-order render tail used to be. The rows are written at 12 B and read into the
    // `(entity, row)` pairs at 8; the pairs are then read once, and that one pass fills every
    // render lane, deleting each pairs bucket as it is consumed. So what stands together is the
    // rows, or the pairs, or all the lanes at `Σ(4 + wᵢ)` a row — the largest of the three, not
    // their sum.
    //
    // **Charged as the sum anyway**, which over-states the phase by the two terms that are not
    // the peak. A term is a file and a window here, and the shape that would say
    // `max(12n, 8n, Σ(4 + wᵢ)n)` is a phase whose terms are alternatives; the model has no such
    // shape, and an over-stated forecast routes a column to extents where it could have kept its
    // arena rather than filling a disk.
    push(
        "the assembly's row partition, 12 B/row — (morton, residual, entity) in .build-tmp/, one \
         view at a time"
            .into(),
        12 * n,
        Phases::ASSEMBLE,
    );
    push(
        "the assembly's (entity, row) partition, 8 B/row, in .build-tmp/ — the permutation's and \
         every render column's input"
            .into(),
        8 * n,
        Phases::ASSEMBLE,
    );
    // **Every render lane, together.** One pass over the pairs fills them all, so they stand full
    // at the same moment: the charge is a record a row in each — the key and the value — summed
    // over the columns the view renders. A group-scoped family renders into the row spaces its
    // scope reaches, at the view that carries the most of them ([`scoped_render_types`]).
    let scoped_lanes = scoped_render_types(args);
    let lane_widths: Vec<u64> = args
        .schema
        .attributes
        .iter()
        .filter(|a| a.render)
        .map(|a| 4 + fixed_width(a.ty))
        .chain(scoped_lanes.iter().map(|&ty| 4 + fixed_width(ty)))
        .collect();
    if !lane_widths.is_empty() {
        let per_row: u64 = lane_widths.iter().sum();
        push(
            format!(
                "the assembly's {} render (row, value) partition(s), {per_row} B/row in all, in \
                 .build-tmp/ — every lane filled by the one pass over the pairs",
                lane_widths.len()
            ),
            per_row * n,
            Phases::ASSEMBLE,
        );
    }

    // ---- the attribute join's staging buffer -------------------------------------------------
    // A second set of columns per attribute source, [`crate::pipeline::JOIN_STAGE_BYTES`] wide in
    // fixed-width slots — but its arenas hold a whole chunk's characters, which that budget does
    // not bound (`pipeline::staged_width` prices a string at a `String` header it no longer costs).
    for source in &args.attribute_sources {
        let columns: Vec<&crate::config::Attribute> = source
            .attributes
            .iter()
            .filter_map(|&i| args.schema.attributes.get(i))
            .collect();
        if columns.is_empty() {
            continue;
        }
        let staged = crate::pipeline::staging_rows(&columns, n) as u64;
        let mut bytes = 0u64;
        for &index in &source.attributes {
            let Some(attribute) = args.schema.attributes.get(index) else {
                continue;
            };
            bytes = bytes
                .saturating_add(fixed_width(attribute.ty).saturating_mul(staged))
                .saturating_add(staged.div_ceil(8));
            if carries_characters(attribute.ty) {
                bytes = bytes
                    .saturating_add(payload(payloads[index], staged))
                    .saturating_add(crate::spill::ARENA_GROWTH_STEP);
            }
        }
        push(
            format!(
                "the attribute join's staging buffer over '{}', {staged} rows a chunk, in \
                 .build-tmp/",
                source.name
            ),
            bytes,
            Phases::JOIN,
        );
    }

    // ---- the bundle, which nothing releases --------------------------------------------------
    push(
        "the entity→term transpose, 4 B/item of offsets and a ceiling of 4 B/pair of terms".into(),
        4 * (n + 1) + 4 * p,
        Phases::BANDS.onwards(),
    );
    push(
        format!(
            "postings.arrow, one record over each of {} term(s) at its own row count — 8 B of \
             Arrow offset and a tag each, then 4 B an entity below 32 of them and a ceiling of \
             Roaring above",
            term_rows.len()
        ),
        postings_bytes(term_rows, n),
        Phases::BANDS.onwards(),
    );
    if args.emit_oracle_pairs {
        push(
            format!(
                "pairs.parquet, at a ceiling of {}.{} B/pair — the test-time oracle's copy of \
                 the relation, which --no-oracle-pairs does not write",
                ORACLE_BYTES_PER_PAIR_TENTHS / 10,
                ORACLE_BYTES_PER_PAIR_TENTHS % 10
            ),
            (p.saturating_mul(ORACLE_BYTES_PER_PAIR_TENTHS)) / 10,
            Phases::BANDS.onwards(),
        );
    }
    if crate::ids::writes_external_ids(args, id_space) {
        // The sidecar is an Arrow `binary` column beside a `u32` entity — a 4 B offset a row and
        // one more at the end, an 8 B `external_id` payload and the entity — and the locator
        // beside it (`ext-locator.u32`) is a `u32` an item. That is 20 B/item of buffer, and the
        // two files measure **20.25** on both bundles that mint them, 26× apart in `n`
        // (`docs/evidence/memos/2026-09-10-disk-bundle-payload.md` §1), so the quarter byte an
        // item the Arrow framing adds is charged with them. ⊘ What is left out is about a
        // kilobyte of schema and footer a file, which is a constant and not a rate.
        //
        // A supplied key's payload is the key's own bytes rather than eight, charged at the mean
        // over the keys this build interned (`crate::ids`). Modelled from the corpus rather than
        // measured, and exact where the keys are a fixed width.
        let payload = match id_space.supplied() {
            None => 8,
            Some(keys) => keys.mean_key_len(),
        };
        push(
            format!(
                "the external-id sidecar and its locator, at {} B/item: a 4 B Arrow offset, a {} \
                 B id and a 4 B entity in the sidecar, a 4 B locator, and a quarter byte an item \
                 of Arrow framing",
                12 + payload,
                payload
            ),
            4 * (n + 1) + (8 + payload) * n + n.div_ceil(4),
            Phases::BANDS.onwards(),
        );
    }
    // **The attribute join's value partitions**, one per fixed-width column it fills: `(entity,
    // value)` at four bytes of entity and the column's own width, standing from the join's first
    // chunk to the replay that writes each bucket into the column as a sequential run. Charged
    // over every item, which is a ceiling — an absent row pushes nothing — and released bucket by
    // bucket at the replay. A string column's lane is charged where its route is known, which is
    // the entity-order model this carries the mapped terms of.
    for attribute in &args.schema.attributes {
        // A string column has no fixed-width lane: it is either an arena, whose `(entity, at)`
        // partition the entity-order model charges, or record-blob extents, which never reach a
        // column at all.
        if carries_characters(attribute.ty) {
            continue;
        }
        let width = fixed_width(attribute.ty);
        push(
            format!(
                "attribute '{}': the join's (entity, value) partition, {} B/item, in .build-tmp/",
                attribute.name,
                4 + width
            ),
            (4 + width) * n,
            Phases::JOIN,
        );
    }
    // **Blob residency and a token index are independent, and a `text` column is both.** A column
    // whose value lives in the record blob still owes `postings.arrow` and `dict.bin` wherever it
    // is indexed (`pipeline::blob_resident`), and those two files are in the bundle from the index
    // phase to the end of the build. Skipping the rest of this loop for a blob-resident column
    // left an indexed `text` column's whole index uncharged: **176.81 B/item on the 10⁷ MedCPT
    // sample, 23% of that bundle**.
    let mut blob_payload = 0u64;
    for (index, attribute) in args.schema.attributes.iter().enumerate() {
        let characters = payload(payloads[index], n);
        if crate::pipeline::blob_resident(&args.schema, attribute) {
            blob_payload = blob_payload.saturating_add(characters);
        }
        if !crate::pipeline::postings_are_owed(&args.schema, attribute) {
            continue;
        }
        if attribute.ty == ScalarType::Text {
            // A `text` column's index is a token index: one posting per `(token, entity)` pair and
            // a dictionary of the tokens, and no value column at all — its `entity → value` answer
            // is the record blob's. [`TEXT_INDEX_SHARE_PERCENT`] carries the figure and what it
            // rests on.
            push(
                format!(
                    "attribute '{}': the text index's postings.arrow and dict.bin, at an \
                     estimate of {TEXT_INDEX_SHARE_PERCENT}% of the column's {} MiB of \
                     characters",
                    attribute.name,
                    characters >> 20
                ),
                characters.saturating_mul(TEXT_INDEX_SHARE_PERCENT) / 100,
                Phases::INDEX.onwards(),
            );
            continue;
        }
        // An indexed keyword column's values file is its dictionary ordinals, four bytes a row;
        // every other family's is its declared width. The presence bitmap beside it is charged at
        // a bit an item, which is a Roaring bitmap's own ceiling.
        let width = match attribute.ty {
            ScalarType::Utf8 | ScalarType::Keyword => 4,
            ty => fixed_width(ty),
        };
        push(
            format!(
                "attribute '{}': its value column at {width} B/item and its presence bitmap",
                attribute.name
            ),
            width * n + n.div_ceil(8),
            Phases::INDEX.onwards(),
        );
        // `values.arrow` is assembled from a spool of the same values beside it
        // (`tessera_filter::values_writer`), so the column's slots stand twice while it is
        // written. Measured on the GBIF prefix: `specieskey` 473,983,152 bytes of spool and `year`
        // 243,206,776, both alive at the index phase's peak.
        push(
            format!(
                "attribute '{}': the value column's own spool, {width} B/item, beside \
                 values.arrow while it is assembled",
                attribute.name
            ),
            width * n,
            Phases::INDEX,
        );
        if matches!(attribute.ty, ScalarType::Utf8 | ScalarType::Keyword) {
            // The dictionary pass's own scratch, under the column's directory rather than
            // `.build-tmp/`: the `(row, ordinal)` partition the merge fills, 8 B a row, and the
            // sorted runs it merges, both gone by the end of the pass. The partition's buckets go
            // back one at a time as the values file is written past them, so this is a ceiling
            // and the pass's own peak is a bucket lower.
            push(
                format!(
                    "attribute '{}': the keyword dictionary's (row, ordinal) partition and \
                     sorted runs, 8 B/item and a ceiling of half the column's characters",
                    attribute.name
                ),
                8 * n + characters / EXTENT_SHARE,
                Phases::INDEX,
            );
            // And the dictionary the pass leaves behind, which is in the bundle from there on.
            // **The characters and the entry overhead beside them**, both at a ceiling: the file
            // is front-coded, so its suffix bytes are at most the column's characters, and each
            // key costs [`DICT_BYTES_PER_KEY_TENTHS`] on top of them. The key count is charged at
            // one a row, which is its own ceiling. Measured at 0.57 of the characters on the 10⁷
            // MedCPT sample's `pmid`, whose eight-character ids are nearly all distinct, and
            // 0.003 on GBIF's `specieskey`, whose 2.6×10⁵ keys repeat over 16.3×10⁶ rows.
            push(
                format!(
                    "attribute '{}': dict.bin, at a ceiling of the column's {} MiB of characters \
                     and {}.{} B a key over one key a row",
                    attribute.name,
                    characters >> 20,
                    DICT_BYTES_PER_KEY_TENTHS / 10,
                    DICT_BYTES_PER_KEY_TENTHS % 10
                ),
                characters.saturating_add(n.saturating_mul(DICT_BYTES_PER_KEY_TENTHS) / 10),
                Phases::INDEX.onwards(),
            );
        }
    }
    // The blocks are charged at half the characters **and half the row framing**: a row is an
    // entity gap and a length, each field a `tag | kind`, plus a `u32` length on a `utf8` one, and
    // for a short value that is more than the value ([`record_framing_bytes`]). The framing is
    // zero exactly where no column is blob-resident, which is where there is no blob. The
    // directory costs nothing an item: it holds a handful of words a block and nothing a row.
    let framing = record_framing_bytes(&args.schema, n);
    if framing > 0 {
        push(
            format!(
                "the record blob: its blocks modelled at half the {} MiB of characters its \
                 columns carry and the {} MiB of row framing around them",
                blob_payload >> 20,
                framing >> 20
            ),
            (blob_payload + framing) / EXTENT_SHARE,
            Phases::BLOB.onwards(),
        );
    }
    // Per view: `morton.u32`, `cuts.u32`, `permutation.bin`, `row-entity.u32` and the segment's
    // `columns.arrow` — the residual, the `tessera_id` and one slot per render column, each with
    // the Arrow validity bitmap the column carries beside it at a bit a row.
    //
    // **`cuts.u32` is charged at its ceiling of 4 B a row**, which is one occupied leaf cell per
    // row. A corpus with several rows to a cell pays a fraction of that — 0.54 B a row over the
    // 25.8M-row GBIF corpus — and the pre-flight has no way to know the cell count before the
    // geometry is read, so the model takes the bound rather than an estimate it would have to
    // apologise for.
    let render: u64 = args
        .schema
        .attributes
        .iter()
        .filter(|a| a.render)
        .map(|a| fixed_width(a.ty))
        .sum();
    let arrow_columns = 2 + args.schema.attributes.iter().filter(|a| a.render).count() as u64;
    push(
        format!("each view's segment, permutation and row→entity files, over {views} view(s)"),
        ((4 + 4 + 4 + 4 + 4 + 8 + render) * n + arrow_columns * n.div_ceil(8)) * views,
        Phases::ASSEMBLE,
    );
    push(
        "the artifact pass's row-column lanes, at a ceiling of 4 B an ordinal over the levels \
         and the views each layer draws on"
            .into(),
        row_column_bytes(args, n, &member_entries_by_layer(args)),
        Phases::ASSEMBLE,
    );
    // **The buckets the pass composes through**, at 8 B a `(row, ordinal)` record. One level at a
    // time and released as each bucket is replayed, so the ceiling is the largest layer's declared
    // entries — a ceiling over its levels rather than a sum of them.
    push(
        "the artifact pass's (row, ordinal) partition buckets, at 8 B a member entry of the \
         largest layer, in .build-tmp/"
            .into(),
        8u64.saturating_mul(
            member_entries_by_layer(args)
                .into_iter()
                .max()
                .unwrap_or(0)
                .max(n),
        ),
        Phases::ASSEMBLE,
    );
    Residency { terms }
}

/// **The line a build prints where the forecast is over the free space**, or `None` where it is
/// not.
///
/// Built here rather than at the call site so it can be tested: it is the only place the model's
/// per-term breakdown reaches an operator, and a warning that had lost it would still be a warning.
///
/// It warns and the build goes on (`crate::pipeline::plan_build` carries why).
pub(crate) fn forecast_warning(disk: &Residency, corpus: Corpus<'_>, free: u64) -> Option<String> {
    let (phase, need) = disk.peak();
    if free >= need {
        return None;
    }
    let Corpus {
        n,
        pair_rows,
        batches,
        ..
    } = corpus;
    Some(format!(
        "warning: this build is forecast to need ~{need} bytes at peak, in the {} phase (n = {n}, \
         pairs = {pair_rows}, batches = {batches}), against {free} available at the output path. \
         The forecast is a model, not a measurement — each term below says whether it is an exact \
         width, a ceiling or an estimate — so the build goes on. If it does run out, the error \
         names the file it could not reserve and the partial bundle is swept. Where that phase's \
         bytes are:{}",
        phase.name(),
        disk.describe_phase(phase)
    ))
}

/// What the artifact pass's row-column lanes cost **one** view.
///
/// **The lane's size depends on which of two forms a level takes, and a flat `4 × n` a level is
/// only one of them.** `tessera_store::derived::choose` picks from the memberships the build has
/// just resolved: a level whose memberships partition the corpus is a label lane (`.tslb`) at one
/// narrow ordinal a row, and one whose memberships overlap is a list lane (`.tsll`) — a `u32`
/// offset a row *and* one ordinal an entry. The 10⁷ MedCPT sample's MeSH list lane is
/// **963,487,236 bytes, 96.35 B/item**, against the 8 a per-item charge gives it.
///
/// **What tells the two apart before the build is the entry count against the entity space.** A
/// level partitions only where no entity is in two of its artifacts, so a layer declaring more
/// than one entry an entity a level has at least one level that does not — and that layer is
/// charged the list form at every level. GBIF's taxonomy declares exactly three entries an
/// occurrence over three levels and is charged the label form, which is what it writes
/// (1,258 MB measured against 1,509 charged); MedCPT's MeSH declares 47 an article over two
/// levels and is charged the list form. A pin settles it outright: `rows` writes no lane at all,
/// `column` is a label lane, `list` a list one.
///
/// ⊘ **The test is necessary and not sufficient**, so a layer whose entries are uneven across its
/// levels — two an entity at one level and none at another — is charged the label form where one
/// of its levels is a list.
///
/// ⊘ **The list charge is a ceiling of about 2× on a level of many artifacts**, and the label
/// charge on a level of few. An ordinal is stored at the narrowest width that addresses the level
/// — one byte under 255 artifacts, two under 65,535, four above — and the artifact count is not
/// known before the level is published.
fn row_column_bytes(args: &crate::BuildArgs, n: u64, entries: &[u64]) -> u64 {
    use tessera_types::layer::ServingLayout;
    let mut bytes = 0u64;
    for (layer, &declared) in args.layers.iter().zip(entries) {
        let views = layer_views(args, layer);
        let levels = layer.levels.len().max(1) as u64;
        // One narrow ordinal a row a level, which is a label lane whole and a list lane's offset
        // table.
        let lanes = 4u64
            .saturating_mul(n)
            .saturating_mul(levels)
            .saturating_mul(views);
        let overlapping = declared > n.saturating_mul(levels);
        let list = match layer.layout {
            Some(ServingLayout::ArtifactMajor) => continue,
            Some(ServingLayout::RowMajorLabel) => false,
            Some(ServingLayout::RowMajorList) => true,
            None => overlapping,
        };
        bytes = bytes.saturating_add(if list {
            lanes.saturating_add(4u64.saturating_mul(declared).saturating_mul(views))
        } else {
            lanes
        });
    }
    bytes
}

/// **How many of this build's views a layer draws on**, which is what its lanes are one of a level.
///
/// Not the length of `views`: a declared name is a plain view **or a whole group**, and a group
/// name draws the layer on every view of it (`Config::expand_layer_views`, `views.md` §2). A layer
/// on a ten-view group declares one name and writes ten lanes a level.
///
/// A name matching no view of this build is charged every view rather than none. The declaration
/// refuses an unknown name long before here (`config`'s layer compilation), so a name that reaches
/// this and resolves to nothing is arguments assembled without the view; a lane charged twice
/// costs a rerun, and one charged at nothing is the ENOSPC the forecast exists to name.
fn layer_views(args: &crate::BuildArgs, layer: &tessera_types::layer::LayerDeclaration) -> u64 {
    let total = args.views.len().max(1) as u64;
    if layer.views.is_empty() {
        return total;
    }
    layer
        .views
        .iter()
        .map(|name| {
            let drawn = args
                .views
                .iter()
                .filter(|view| {
                    // The view's own id, or the group's — a group name draws the layer on every
                    // view of it, and `<group>` is the first component of `group:key`.
                    &view.view_id == name
                        || tessera_store::view_path_components(&view.view_id)
                            .first()
                            .is_some_and(|head| head == name)
                })
                .count() as u64;
            match drawn {
                0 => total,
                drawn => drawn,
            }
        })
        .sum()
}

/// Each declared layer's `(artifact, source)` pairs, in declaration order — its member source's
/// key values, or zero for a layer that declares no member file.
fn member_entries_by_layer(args: &crate::BuildArgs) -> Vec<u64> {
    args.layers
        .iter()
        .map(|layer| {
            args.layer_inputs
                .iter()
                .find(|input| input.name == layer.name)
                .and_then(|input| input.members.as_ref())
                .map_or(0, |members| member_entries(members).0)
        })
        .collect()
}

/// The characters one item of each declared column carries, indexed by declaration position —
/// **sampled once**, because both models want it and the sample decodes rows.
///
/// Zero for a fixed-width column, which carries none, and for one whose source will not open: an
/// input the pre-flight cannot read contributes nothing rather than refusing the build.
pub(crate) fn payloads_per_item(args: &crate::BuildArgs) -> Vec<f64> {
    let mut payloads = vec![0.0f64; args.schema.attributes.len()];
    for source in &args.attribute_sources {
        let Some(metadata) = footer(&source.path) else {
            continue;
        };
        let rows = metadata.file_metadata().num_rows().max(0) as u64;
        for &index in &source.attributes {
            let Some(attribute) = args.schema.attributes.get(index) else {
                continue;
            };
            if !carries_characters(attribute.ty) {
                continue;
            }
            let column = attribute.column();
            // The sample, and the footer's own figure where the file will not give one.
            let per_item = sampled_bytes_per_item(&source.path, column).unwrap_or_else(|| {
                let footer_bytes = uncompressed_column_bytes(&metadata, column);
                if rows == 0 {
                    0.0
                } else {
                    footer_bytes as f64 / rows as f64
                }
            });
            payloads[index] += per_item;
        }
    }
    payloads
}

/// `per_item` characters over `rows` items, rounded.
fn payload(per_item: f64, rows: u64) -> u64 {
    (per_item * rows as f64).round() as u64
}

/// Row groups sampled per string column, spread across the file rather than taken from its head:
/// a corpus in its publisher's export order has no reason to be uniform, and GBIF is not.
const PAYLOAD_SAMPLE_GROUPS: usize = 3;

/// Rows read from each sampled row group. 700,000 over three groups is at most ~2.1×10⁶ values
/// decoded per string column, whatever the corpus — a bounded read against a build that otherwise
/// discovers the same figure by filling the disk with it.
const PAYLOAD_SAMPLE_ROWS: usize = 700_000;

/// The mean bytes one row of a string column carries, or `None` where the file will not say.
///
/// The characters alone: a null row contributes nothing and is counted in the denominator, so the
/// figure already carries the column's null rate and a caller multiplies by `n`.
fn sampled_bytes_per_item(path: &std::path::Path, column: &str) -> Option<f64> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let groups = {
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).ok()?).ok()?;
        let count = builder.metadata().num_row_groups();
        if count == 0 {
            return None;
        }
        let take = PAYLOAD_SAMPLE_GROUPS.min(count);
        (0..take).map(|i| i * count / take).collect::<Vec<_>>()
    };
    let mut bytes = 0u64;
    let mut rows = 0u64;
    for group in groups {
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).ok()?).ok()?;
        let root = builder
            .schema()
            .fields()
            .iter()
            .position(|f| f.name() == column)?;
        let projection = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), [root]);
        let reader = builder
            .with_row_groups(vec![group])
            .with_projection(projection)
            .with_limit(PAYLOAD_SAMPLE_ROWS)
            .with_batch_size(65_536)
            .build()
            .ok()?;
        for batch in reader {
            let batch = batch.ok()?;
            let array = batch.column(0);
            rows += batch.num_rows() as u64;
            bytes += string_bytes(array)?;
        }
    }
    (rows > 0).then(|| bytes as f64 / rows as f64)
}

/// The characters one decoded array holds, or `None` for an array that is not a string one — a
/// declared `keyword` over a source column of another type, which the attribute join refuses on its
/// own terms and this declines to price rather than guess at.
fn string_bytes(array: &arrow::array::ArrayRef) -> Option<u64> {
    use arrow::array::Array;
    let offsets_span = |first: i64, last: i64| (last - first).max(0) as u64;
    match array.data_type() {
        arrow::datatypes::DataType::Utf8 => {
            let a = array
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("a Utf8 array downcasts to StringArray");
            let offsets = a.value_offsets();
            Some(offsets_span(offsets[0] as i64, offsets[a.len()] as i64))
        }
        arrow::datatypes::DataType::LargeUtf8 => {
            let a = array
                .as_any()
                .downcast_ref::<arrow::array::LargeStringArray>()
                .expect("a LargeUtf8 array downcasts to LargeStringArray");
            let offsets = a.value_offsets();
            Some(offsets_span(offsets[0], offsets[a.len()]))
        }
        _ => None,
    }
}

/// One member source's `(artifact, source)` pairs — its **key values** — and its rows beside them.
///
/// ⊘ **The row count was the only denominator until 2026-09-10 and it is the wrong one for the disk
/// terms.** A member row's `key` column is a list, so a row is one pair on a flat layer and one per
/// level on a tiered one. GBIF's member file is 3,495,729,729 rows carrying 10,014,654,968 pairs,
/// and the spill and the packed extents were each charged at 2.9× less than the build then wrote.
/// The rows are still the right denominator for the publication's own window, which is one level.
///
/// The leaf's `num_values` counts a value at every level of repetition, so a row whose key list is
/// short is counted at its own length rather than at the level count. A row naming no artifact at
/// all is over-counted where the writer stores an empty list as one null value, which is the
/// direction this whole module is loose in.
fn member_entries(members: &crate::config::MemberSource) -> (u64, u64) {
    let Some(metadata) = footer(&members.path) else {
        return (0, 0);
    };
    let rows = metadata.file_metadata().num_rows().max(0) as u64;
    let key = members.fields.of("key");
    let mut values = 0u64;
    for group in metadata.row_groups() {
        for chunk in group.columns() {
            if chunk.column_path().parts().first().map(String::as_str) == Some(key) {
                values = values.saturating_add(chunk.num_values().max(0) as u64);
            }
        }
    }
    // A file whose key column is not named as declared prices at its rows, which is what the
    // footer can still say. The join refuses the file on its own terms.
    (if values == 0 { rows } else { values }, rows)
}

/// One Parquet file's metadata, or `None` where it cannot be had.
fn footer(
    path: &std::path::Path,
) -> Option<std::sync::Arc<parquet::file::metadata::ParquetMetaData>> {
    let file = std::fs::File::open(path).ok()?;
    let builder =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file).ok()?;
    Some(builder.metadata().clone())
}

/// The uncompressed bytes one named column occupies across every row group.
fn uncompressed_column_bytes(
    metadata: &parquet::file::metadata::ParquetMetaData,
    column: &str,
) -> u64 {
    let mut total = 0u64;
    for group in metadata.row_groups() {
        for chunk in group.columns() {
            if chunk.column_path().parts().first().map(String::as_str) == Some(column) {
                total = total.saturating_add(chunk.uncompressed_size().max(0) as u64);
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model at a budget no publication batch can be the binding term of, which is what every
    /// test below wants: they read the per-entry arithmetic, and the batch sizing is
    /// [`crate::layers::publication_batch_entries`]'s own to test.
    #[allow(clippy::too_many_arguments)]
    fn choose_routes(
        schema: &crate::config::Schema,
        n: u64,
        ids: IdShape,
        columns: Vec<ColumnCost>,
        entries: u64,
        level_entries: u64,
        free: Option<u64>,
    ) -> (crate::pipeline::ColumnRoutes, Residency) {
        super::choose_routes(
            schema,
            n,
            ids,
            columns,
            // No group-scoped family: the fixtures here are a schema, and a scoped column's lanes
            // are `plan_routes`' own input from the view declarations.
            &[],
            entries,
            level_entries,
            // Every layer's entries as one layer's, which is exact for a single-layer fixture and
            // over-charges the artifact pass for a many-layer one.
            entries,
            free,
            u64::MAX,
        )
    }

    fn entity_order_residency(
        n: u64,
        ids: IdShape,
        columns: &[ColumnCost],
        member_entries: u64,
        level_entries: u64,
    ) -> Residency {
        super::entity_order_residency(
            n,
            ids,
            columns,
            // As above: the fixtures declare no group-scoped render family.
            &[],
            member_entries,
            level_entries,
            // As above: one layer, so the largest layer's entries are all of them.
            member_entries,
            u64::MAX,
        )
    }

    /// A column whose characters, if it has any, fill an arena — the shape a string column keeps
    /// while some pass reads it at an entity.
    fn column(ty: ScalarType, payload: u64) -> ColumnCost {
        ColumnCost {
            ty,
            payload_bytes: payload,
            extents: false,
            framing_bytes: 0,
            text_index: false,
            render: false,
            phases: Phases::JOIN.and(Phases::INDEX).and(Phases::BLOB),
        }
    }

    /// The same column, spilled as record-blob extents instead
    /// ([`crate::pipeline::takes_extents`]).
    fn spilled(ty: ScalarType, payload: u64) -> ColumnCost {
        ColumnCost {
            extents: true,
            ..column(ty, payload)
        }
    }

    /// **The campaign's own schema at its own scales.** Seven declared columns — `u64`, `u32`,
    /// `timestamp_us`, a `u8` category, a `keyword`, a `text` and a `u32` — over the generator's
    /// three member tables (2n + 3.4n + n rows). The point of the test is the shape: the tail is
    /// linear in `n` and dominated by the two variable-width columns and the member tables, not by
    /// anything the batch stride can reach.
    fn campaign_residency(n: u64, text_bytes_per_item: u64) -> Residency {
        let columns = [
            column(ScalarType::U64, 0),
            column(ScalarType::U32, 0),
            column(ScalarType::TimestampUs, 0),
            column(ScalarType::U8, 0),
            column(ScalarType::Keyword, 8 * n),
            spilled(ScalarType::Text, text_bytes_per_item * n),
            column(ScalarType::U32, 0),
        ];
        let entries = (2 * n) + (34 * n / 10) + n;
        entity_order_residency(n, IdShape::dense(n), &columns, entries, entries)
    }

    /// Every anonymous term that is a **rate** over the corpus.
    fn scaling_total(residency: &Residency) -> u64 {
        residency
            .terms
            .iter()
            .filter(|t| !t.mapped && !t.constant)
            .map(|t| t.bytes)
            .sum()
    }

    #[test]
    fn the_tail_is_linear_in_the_item_count_and_the_batch_stride_reaches_none_of_it() {
        let small = campaign_residency(10_000_000, 200);
        let large = campaign_residency(50_000_000, 200);
        // Five times the corpus, five times the residency — exactly, net of the terms that are not
        // a function of the corpus at all: the allocator slack, and the partitions, whose writer
        // buffers and bucket the key type bounds rather than the row count.
        let ratio = scaling_total(&large) as f64 / scaling_total(&small) as f64;
        assert!(
            (4.99..5.01).contains(&ratio),
            "the tail should scale with the corpus, got {ratio}"
        );
    }

    /// **The sorted source ids are reported and not charged.** They are a file under
    /// `.build-tmp/`, so they appear in the breakdown at their full size — 8 B an item, 26.0 GiB
    /// at the GBIF rung — and add nothing to the figure `--memory-budget` is compared against.
    /// Charged, they were almost the whole of that figure at rung 6: 26,670 MiB of the 26,734 the
    /// model asked a 47 GiB machine for. It asks 13,399 now, and the ids are on the disk.
    #[test]
    fn the_source_ids_are_reported_as_mapped_and_charged_at_nothing() {
        let n = 1_000_000_u64;
        let with_layer = entity_order_residency(n, IdShape::dense(n), &[], n, n);
        let without = entity_order_residency(n, IdShape::dense(n), &[], 0, 0);

        // The publication is the only charged term either way; the ids move the mapped figure and
        // not the charged one.
        assert_eq!(
            with_layer.total() - without.total(),
            n * BYTES_PER_MEMBER_ENTRY,
            "the publication's Roaring is what a layer adds to the charged figure"
        );
        let ids = with_layer
            .terms
            .iter()
            .find(|t| t.what.contains("the sorted source ids"))
            .expect("the ids are a term of their own");
        assert_eq!(ids.bytes, 8 * n);
        assert!(
            ids.mapped,
            "the ids are a file, not memory the machine must have"
        );
        assert!(
            with_layer
                .describe()
                .contains("MiB (mapped)  the sorted source ids"),
            "a refusal has to show the disk the build wants: {}",
            with_layer.describe()
        );
    }

    /// **The ids file is charged over the rows every view declares, not over the entity space.**
    /// Pass one allocates the union at the sum of the views' row counts and dedups inside it, so a
    /// corpus whose views overlap holds a file larger than the `n` it produces —
    /// 1.76× on `treeoflife-1m` and 4.29× on `multiview`.
    #[test]
    fn the_source_ids_are_charged_over_the_slots_the_file_is_allocated_at() {
        let n = 1_000_000;
        let dense = entity_order_residency(n, IdShape::dense(n), &[], 0, 0);
        let overlapping = entity_order_residency(
            n,
            IdShape {
                slots: 4 * n,
                max_id: n - 1,
            },
            &[],
            0,
            0,
        );
        assert_eq!(
            overlapping.at(Phase::Spill) - dense.at(Phase::Spill),
            8 * 3 * n,
            "four views over one entity space cost four ids files, not one"
        );
    }

    /// **A build that wrote no ids array is charged nothing for one**, and the term disappears from
    /// what a refusal prints rather than standing at zero. Pass one proves the union is one
    /// unbroken range from a presence bitmap and writes no array
    /// ([`crate::pipeline::SourceIds`]), which is every corpus on the ladder but `multiview`.
    #[test]
    fn a_proved_range_is_charged_no_ids_file_at_all() {
        let n = 1_000_000;
        let held = entity_order_residency(n, IdShape::dense(n), &[], 0, 0);
        let ranged = entity_order_residency(
            n,
            IdShape {
                slots: 0,
                max_id: n - 1,
            },
            &[],
            0,
            0,
        );
        assert_eq!(
            held.at(Phase::Spill) - ranged.at(Phase::Spill),
            8 * n,
            "the array is the whole difference between the two routes"
        );
        assert!(
            !ranged.describe().contains("the sorted source ids"),
            "a term at zero bytes is not a term: {}",
            ranged.describe()
        );
    }

    /// **The member spill's runs widen with the largest source id.** The runs encode source ids
    /// and the merged table entities, so a corpus whose ids are hashes over the `u64` space spends
    /// the extra varint bytes on the runs alone.
    #[test]
    fn the_member_spill_widens_with_a_sparse_id_space() {
        let n = 1_000_000;
        let entries = 4_000_000;
        let dense = entity_order_residency(n, IdShape::dense(n), &[], entries, entries);
        let hashed = entity_order_residency(
            n,
            IdShape {
                slots: n,
                max_id: u64::MAX,
            },
            &[],
            entries,
            entries,
        );
        // varint_len(u64::MAX) = 10 against varint_len(10^6) = 3.
        assert_eq!(
            hashed.at(Phase::Join) - dense.at(Phase::Join),
            entries * 7,
            "a delta over the whole u64 space is ten bytes where one over 10^6 entities is three"
        );
        assert_eq!(
            hashed.total(),
            dense.total(),
            "the spill is a file either way"
        );
    }

    /// **A declared column is reported and not charged.** The whole of a keyword column — its
    /// per-entity offsets, its presence bits and every character it holds — is a file under
    /// `.build-tmp/`, so it appears in the breakdown at its full size and adds nothing to the
    /// figure `--memory-budget` is compared against.
    #[test]
    fn a_declared_column_is_reported_as_mapped_and_charged_at_nothing() {
        let n = 10_000_000;
        let bare = entity_order_residency(n, IdShape::dense(n), &[], 0, 0);
        let with_keyword = entity_order_residency(
            n,
            IdShape::dense(n),
            &[column(ScalarType::Keyword, 400 * n)],
            0,
            0,
        );
        // **Net of the partitions the column opens.** Its storage is a file and is charged at
        // nothing; what a declared column does cost the machine is the join's `(entity, value)`
        // partition and, for a keyword one, the dictionary's `(row, ordinal)` partition — both
        // constants of the key type, both named terms of their own.
        assert_eq!(scaling_total(&with_keyword), scaling_total(&bare));
        let term = with_keyword
            .terms
            .iter()
            .find(|t| t.what.contains("declared column"))
            .expect("the column is a term of its own");
        // The offsets, the presence bits, and the arena the characters fill. A `keyword` record
        // has no header: its length is packed into the offset word (`column.rs`, `RecordShape`).
        assert_eq!(term.bytes, 8 * n + n.div_ceil(8) + arena_capacity(400 * n));
        assert!(
            with_keyword.describe().contains("(mapped)"),
            "the breakdown must say which terms are files: {}",
            with_keyword.describe()
        );
    }

    /// **A spilled column costs its extents and nothing else.** Its characters are written once
    /// as record-blob blocks under `.build-tmp/`, charged at half the source's characters
    /// ([`EXTENT_SHARE`]); the column's own slot is a length in memory and no file at all — not
    /// even the presence bitmap, which the join has no lane to mark
    /// ([`crate::column::EntityColumn::spilled`]). That holds for every string type the route
    /// takes, and what it removes on a `keyword` column is the offset array as well as the
    /// arena — 8.125 B/item before a character.
    #[test]
    fn a_spilled_column_costs_its_extents_and_not_an_arena() {
        let n = 10_000_000;
        let payload = 400 * n;
        for ty in [ScalarType::Text, ScalarType::Keyword, ScalarType::Utf8] {
            let route = entity_order_residency(n, IdShape::dense(n), &[spilled(ty, payload)], 0, 0);
            let term = route
                .terms
                .iter()
                .find(|t| t.what.contains("declared column"))
                .expect("the column is a term of its own");
            assert_eq!(term.bytes, payload / 2, "{ty:?}");
            assert!(
                term.what.contains("MiB of extents"),
                "the breakdown must name them: {}",
                term.what
            );
            // The arena route on the same column, for the difference the routing is worth.
            let arena = entity_order_residency(n, IdShape::dense(n), &[column(ty, payload)], 0, 0);
            let held = arena
                .terms
                .iter()
                .find(|t| t.what.contains("declared column"))
                .expect("the column is a term of its own");
            assert!(
                held.bytes > term.bytes + 8 * n,
                "{ty:?}: the arena route costs the offsets and the characters whole"
            );
            // And the frame around each extent row, which goes through the same compression the
            // characters do ([`extent_framing_bytes`]).
            let framed = entity_order_residency(
                n,
                IdShape::dense(n),
                &[ColumnCost {
                    framing_bytes: extent_framing_bytes(n),
                    ..spilled(ty, payload)
                }],
                0,
                0,
            );
            let with_frame = framed
                .terms
                .iter()
                .find(|t| t.what.contains("declared column"))
                .expect("the column is a term of its own");
            assert_eq!(
                with_frame.bytes,
                term.bytes + extent_framing_bytes(n) / 2,
                "{ty:?}: 15 B a row of frame, charged at half with the characters"
            );
        }
    }

    /// **The row framing is charged from the route, not from the declared type.** A `keyword`
    /// column the record blob alone reads spills the same extents a `text` column does and pays
    /// the same 15 bytes a row, and the same column on the arena route pays none of it — its
    /// values are not framed until the record blob itself.
    ///
    /// Kills a model that tests `ty == Text` for the frame, which leaves an unindexed `keyword`
    /// column's rows uncharged on the route that column now takes.
    #[test]
    fn the_row_framing_follows_the_route_and_not_the_type() {
        let n = 2_000;
        let (mut args, _temp) = fixture(n);
        let code = args
            .schema
            .attributes
            .iter()
            .position(|a| a.name == "code")
            .expect("the fixture declares a keyword column");
        // With neither `index` nor `render` the column is read by the record blob alone, which is
        // the shape the extent route exists for.
        args.schema.attributes[code].index = false;
        let payloads = payloads_per_item(&args);

        let spilled = crate::pipeline::ColumnRoutes::every_available(&args.schema);
        assert!(
            spilled.takes_extents(code),
            "an unflagged keyword column has the extent route"
        );
        let (columns, _, _, _) = model_inputs(&args, n, &payloads, &spilled);
        assert_eq!(columns[code].framing_bytes, extent_framing_bytes(n));
        let weight = args
            .schema
            .attributes
            .iter()
            .position(|a| a.name == "weight")
            .expect("the fixture declares a render column");
        assert_eq!(
            columns[weight].framing_bytes, 0,
            "a fixed-width column carries no extents and no frame around them"
        );

        let arena = crate::pipeline::ColumnRoutes::forced_only(&args.schema);
        let (held, _, _, _) = model_inputs(&args, n, &payloads, &arena);
        assert_eq!(
            held[code].framing_bytes, 0,
            "a column that keeps its arena is not framed until the record blob itself"
        );
    }

    /// A schema of string columns, each named by its position, for the route tests. `index` is the
    /// declaration flag, so the second element of a case decides whether the column is offered the
    /// extent route at all.
    fn route_schema(columns: &[(ScalarType, bool)]) -> crate::config::Schema {
        crate::config::Schema {
            attributes: columns
                .iter()
                .enumerate()
                .map(|(index, &(ty, indexed))| crate::config::Attribute {
                    name: format!("column{index}"),
                    field: None,
                    title: None,
                    ty,
                    analyser: None,
                    vocabulary: None,
                    value_set: None,
                    index: indexed,
                    render: false,
                })
                .collect(),
            vocabularies: Default::default(),
        }
    }

    /// **The route is the modelled arena against the free space.** A column the record blob alone
    /// reads fills an arena while the entity-order stages have the disk for one and spills its
    /// characters as record-blob extents when they do not, which is the whole of the rule
    /// (`build-column-extents.md` §2).
    ///
    /// The two figures straddle the whole window rather than the arena: what the column is charged
    /// against is every file standing beside it, and at 10⁷ items the source ids and the
    /// ordinal→entity map are 120 MB of that before a character.
    #[test]
    fn a_string_column_fills_an_arena_while_one_fits_and_spills_when_it_does_not() {
        let n = 10_000_000;
        let payload = 400 * n;
        let schema = route_schema(&[(ScalarType::Keyword, false)]);
        let scratch_with_arena = stage_scratch(&entity_order_residency(
            n,
            IdShape::dense(n),
            &[column(ScalarType::Keyword, payload)],
            0,
            0,
        ));

        let fits = choose_routes(
            &schema,
            n,
            IdShape::dense(n),
            vec![spilled(ScalarType::Keyword, payload)],
            0,
            0,
            Some(scratch_with_arena * ROUTE_HEADROOM),
        );
        assert!(
            !fits.0.takes_extents(0),
            "a window inside the free space's headroom keeps the arena"
        );

        let does_not = choose_routes(
            &schema,
            n,
            IdShape::dense(n),
            vec![spilled(ScalarType::Keyword, payload)],
            0,
            0,
            Some(scratch_with_arena * ROUTE_HEADROOM - 1),
        );
        assert!(
            does_not.0.takes_extents(0),
            "a window over the free space's headroom spills"
        );
        // And the residency the caller carries away is the one the routes describe, not the
        // all-spilled model the choice started from.
        assert!(stage_scratch(&fits.1) > stage_scratch(&does_not.1));
    }

    /// **A filesystem that will not say how much is free spills.** The arena is the larger
    /// footprint and the one a build runs out of disk on, so the route with nothing to go on takes
    /// the smaller.
    #[test]
    fn an_unreadable_filesystem_spills_every_column_it_can() {
        let n = 1_000_000;
        let schema = route_schema(&[(ScalarType::Keyword, false), (ScalarType::Utf8, false)]);
        let columns = vec![
            spilled(ScalarType::Keyword, 400 * n),
            spilled(ScalarType::Utf8, 400 * n),
        ];
        let (routes, _) = choose_routes(&schema, n, IdShape::dense(n), columns, 0, 0, None);
        assert!(routes.takes_extents(0) && routes.takes_extents(1));
    }

    /// **`text` spills however much disk there is** (`build-column-extents.md` §2). Its arena is
    /// filled by a second decode of the source in entity order, and that cost is the source
    /// permutation rather than the arena's size, so no amount of space buys it back. An indexed
    /// `keyword` beside it has two routes like every other bundle-wide string column, and space
    /// buys it the arena.
    #[test]
    fn text_ignores_the_free_space_and_an_indexed_keyword_does_not() {
        let n = 1_000_000;
        let payload = 400 * n;
        let schema = route_schema(&[(ScalarType::Text, true), (ScalarType::Keyword, true)]);
        for free in [1 << 20, 1 << 30, 1 << 40] {
            let columns = vec![
                spilled(ScalarType::Text, payload),
                spilled(ScalarType::Keyword, payload),
            ];
            let (routes, _) =
                choose_routes(&schema, n, IdShape::dense(n), columns, 0, 0, Some(free));
            assert!(
                routes.takes_extents(0),
                "text spills with {free} bytes free"
            );
            assert_eq!(
                routes.takes_extents(1),
                free < 1 << 40,
                "an indexed keyword's route with {free} bytes free"
            );
        }
    }

    /// **Two arenas that each fit alone need not fit together.** A column is promoted against the
    /// window the promotions before it already bought, so the space is spent once and not once per
    /// column. Declaration order decides which one gets it, which is what makes the answer the
    /// same on two runs over one corpus.
    #[test]
    fn a_second_arena_is_spilled_where_the_space_only_covers_one() {
        let n = 10_000_000;
        let payload = 400 * n;
        let schema = route_schema(&[(ScalarType::Keyword, false), (ScalarType::Utf8, false)]);
        let one = stage_scratch(&entity_order_residency(
            n,
            IdShape::dense(n),
            &[
                column(ScalarType::Keyword, payload),
                spilled(ScalarType::Utf8, payload),
            ],
            0,
            0,
        ));
        let both = stage_scratch(&entity_order_residency(
            n,
            IdShape::dense(n),
            &[
                column(ScalarType::Keyword, payload),
                column(ScalarType::Utf8, payload),
            ],
            0,
            0,
        ));
        assert!(one < both, "the fixture has to straddle something");
        let columns = vec![
            spilled(ScalarType::Keyword, payload),
            spilled(ScalarType::Utf8, payload),
        ];
        let (routes, _) = choose_routes(
            &schema,
            n,
            IdShape::dense(n),
            columns,
            0,
            0,
            Some(one * ROUTE_HEADROOM),
        );
        assert!(!routes.takes_extents(0), "the first column takes the arena");
        assert!(
            routes.takes_extents(1),
            "the second is charged against the window the first left"
        );
    }

    /// **The rung-6 forecast fits the budget that build runs under.**
    ///
    /// 3.5×10⁹ rows and entities over the GBIF declaration's four columns and three member rows an
    /// item, priced against the 24 GiB budget the rung is built under. The pre-flight refuses a
    /// build whose anonymous total exceeds the budget (`crate::pipeline`), so this is that refusal
    /// read at the model: under the bound is a build that starts.
    ///
    /// The term this test exists for is the term images'. It is the worker count times three
    /// bitmaps of a bitset container per 65,536 values, so at the machine's own width on a
    /// twelve-core box it came to 15,002 MiB by itself and put the forecast over the budget.
    /// `crate::term_images_pass::derive_threads` caps the width, and this asserts the consequence
    /// rather than the cap.
    #[test]
    fn the_rung_six_forecast_fits_the_twenty_four_gibibyte_budget() {
        const BUDGET: u64 = 24 << 30;
        const N: u64 = 3_495_729_729;
        // The declaration's columns at the characters an item the corpus measures, as
        // `the_gbif_rung_spills_where_the_slice_that_fits_does_not` states them.
        let columns = vec![
            column(ScalarType::U8, 0),
            column(ScalarType::Keyword, (6.60 * N as f64) as u64),
            column(ScalarType::U16, 0),
            spilled(ScalarType::Keyword, (31.22 * N as f64) as u64),
        ];
        let schema = route_schema(&[
            (ScalarType::U8, false),
            (ScalarType::Keyword, true),
            (ScalarType::U16, false),
            (ScalarType::Keyword, false),
        ]);
        // 459 GB of disk, which is what the box the rung was attempted on has.
        let (_routes, residency) = super::choose_routes(
            &schema,
            N,
            IdShape::dense(N),
            columns,
            &[],
            3 * N,
            3 * N,
            3 * N,
            Some(459_000_000_000),
            BUDGET,
        );
        let total = residency.total();
        assert!(
            total <= BUDGET,
            "the rung-6 forecast is {} MiB against a {} MiB budget, so the build is refused              before it starts:{}",
            total >> 20,
            BUDGET >> 20,
            residency.describe()
        );
        // The term images are a real share of it and not a term that rounded to nothing: a model
        // charging them at zero would pass the bound above for the wrong reason.
        let images: u64 = residency
            .terms
            .iter()
            .filter(|term| term.what.starts_with("the term images"))
            .map(|term| term.bytes)
            .sum();
        assert_eq!(
            images,
            crate::term_images_pass::derive_threads() as u64
                * (term_image_bitmap_bytes(N)
                    + tessera_store::permutation::project_scratch_bound(N, N).total()),
            "the two term-image terms must be the pass's own width times what one worker holds"
        );
        assert!(
            images > 1 << 30,
            "the term images are {} MiB, which is too small for this bound to be about them",
            images >> 20
        );
    }

    /// **The rung the route exists for, at the schema it exists for.** GBIF's `scientificname` is
    /// a `keyword` with neither `index` nor `render`, measured at 31.22 characters an item over
    /// 125,789,091 occurrences, beside an indexed `keyword` at 6.60 and three member rows an item
    /// (`probes/2026-09-10-blob-resident-strings/`). At 3,495,729,729 rows its arena is 155 GB
    /// (modelled) and the entity-order stages want about 365 GB of scratch around it, which is
    /// more than the box has — so the column spills, which is the case the extent route was built
    /// for. The same schema at 1.26×10⁸ rows fits with two orders of magnitude to spare and keeps
    /// its arena.
    #[test]
    fn the_gbif_rung_spills_where_the_slice_that_fits_does_not() {
        // The two string columns and the fixed-width ones, at the characters an item the corpus
        // measures. Positions follow `data/ladder/gbif`'s declaration: category, indexed keyword,
        // u16, blob-resident keyword.
        let gbif = |n: u64| {
            vec![
                column(ScalarType::U8, 0),
                column(ScalarType::Keyword, (6.60 * n as f64) as u64),
                column(ScalarType::U16, 0),
                spilled(ScalarType::Keyword, (31.22 * n as f64) as u64),
            ]
        };
        let schema = route_schema(&[
            (ScalarType::U8, false),
            (ScalarType::Keyword, true),
            (ScalarType::U16, false),
            (ScalarType::Keyword, false),
        ]);
        // 459 GB of disk, which is what the box the rung was attempted on has.
        let free = Some(459_000_000_000);
        for (n, spills) in [(125_789_091u64, false), (3_495_729_729u64, true)] {
            let (routes, _) = choose_routes(&schema, n, IdShape::dense(n), gbif(n), 3 * n, n, free);
            assert_eq!(
                routes.takes_extents(3),
                spills,
                "at {n} rows the blob-resident keyword takes the wrong route"
            );
        }
    }

    /// **A text index costs a second set of files, at the same time as the first.** The runs spill
    /// while the column they are tokenised from is still resident, so the disk pre-flight's column
    /// phase has to see both — and neither may reach the memory figure.
    #[test]
    fn a_text_index_carries_its_runs_beside_the_column() {
        let n = 10_000_000;
        let plain = spilled(ScalarType::Text, 400 * n);
        let indexed = ColumnCost {
            text_index: true,
            ..plain
        };
        let extent_bytes = 200 * n;
        // The source ids and the ordinal→entity map are files whatever the schema declares, so
        // each figure below is stated against a build declaring no column at all.
        let bare = entity_order_residency(n, IdShape::dense(n), &[], 0, 0).at(Phase::Index);

        let without = entity_order_residency(n, IdShape::dense(n), &[plain], 0, 0);
        assert_eq!(without.at(Phase::Index) - bare, extent_bytes);

        // The runs are charged the source's characters, being uncompressed where the extents that
        // hold the same prose are not.
        let with = entity_order_residency(n, IdShape::dense(n), &[indexed], 0, 0);
        assert_eq!(with.at(Phase::Index) - bare, extent_bytes + 400 * n);
        assert_eq!(
            with.total(),
            without.total(),
            "neither is charged to memory"
        );
        assert!(
            with.describe().contains("sorted runs"),
            "the runs must be a named term of their own: {}",
            with.describe()
        );
    }

    /// **The publication's Roaring is charged and every file beside it is reported.** A member row
    /// is a heap bitmap only while its level is being published, a delta in two files under
    /// `.build-tmp/`, and the packed extent the store then reads it back through — and only the
    /// first is memory the machine must have. A model that kept charging the others would refuse
    /// builds that now fit, which is the failure mode of carrying a cost model past the thing it
    /// modelled.
    #[test]
    fn a_member_row_is_charged_where_it_is_resident_and_reported_where_it_is_a_file() {
        let n = 10_000_000;
        let rows = 64_000_000;
        let without = entity_order_residency(n, IdShape::dense(n), &[], 0, 0);
        let with = entity_order_residency(n, IdShape::dense(n), &[], rows, rows);
        // The Roaring copies, and the artifact pass's bucket, which holds one record per member
        // entry in its row range and so rises with the layer's entries where the rest of the
        // model's partitions rise with the rows alone: 16 B a record over 128 buckets, net of the
        // row-sized bucket a build with no members already charges.
        let pass_bucket = 16
            * (rows / crate::spill::PARTITION_BUCKETS as u64
                - n / crate::spill::PARTITION_BUCKETS as u64);
        assert_eq!(
            with.total() - without.total(),
            rows * BYTES_PER_MEMBER_ENTRY + pass_bucket,
            "only the Roaring copies and the artifact pass's bucket are memory"
        );
        assert_eq!(
            with.at(Phase::Join) - without.at(Phase::Join),
            rows * (SPILLED_BYTES_PER_MEMBER_ENTRY + MAPPED_BYTES_PER_MEMBER_ENTRY),
            "the runs, the merged table and the extent the store reads through are all disk"
        );
        assert!(
            with.describe().contains("packed extent"),
            "the mapped memberships must be a named term of their own: {}",
            with.describe()
        );
        assert!(
            with.describe().contains("member spill"),
            "the spill must be a named term of its own: {}",
            with.describe()
        );
    }

    /// **A phase is charged what stands through it and not what the build ever writes.** The
    /// sorted source ids go back at the layer publication, so they are the join phase's and no
    /// later phase's; the packed member extents are written there and never released, so they are
    /// that phase's and every one after it.
    #[test]
    fn a_term_is_charged_to_the_phases_it_stands_through() {
        let n = 10_000_000;
        let entries = 64_000_000;
        let residency = entity_order_residency(n, IdShape::dense(n), &[], entries, entries);

        let ids = 8 * n;
        assert_eq!(
            residency.at(Phase::Join) - residency.at(Phase::Index),
            ids + entries * SPILLED_BYTES_PER_MEMBER_ENTRY,
            "the ids and the member spill are the join's and not the index's"
        );
        assert_eq!(
            residency.at(Phase::Index),
            residency.at(Phase::Assemble),
            "what is left after the join is the map and the extents, and neither is released"
        );
        assert_eq!(
            residency.at(Phase::Spill),
            ids,
            "before the assignment the map does not exist"
        );
    }

    /// **A column is charged to its last reader's phase.** A render column is read by the segment
    /// write, a blob-resident one by the record blob, and a column that is neither has met its last
    /// reader when the filter postings end (`pipeline::write_filter_postings`).
    #[test]
    fn a_column_is_charged_to_the_phase_its_last_reader_is_in() {
        let n = 10_000_000;
        let indexed = ColumnCost {
            phases: Phases::JOIN.and(Phases::INDEX),
            ..column(ScalarType::Keyword, 40 * n)
        };
        let rendered = ColumnCost {
            phases: Phases::JOIN.onwards(),
            ..column(ScalarType::U32, 0)
        };
        let bare = entity_order_residency(n, IdShape::dense(n), &[], 0, 0);
        let with = entity_order_residency(n, IdShape::dense(n), &[indexed, rendered], 0, 0);

        let index_only = 8 * n + n.div_ceil(8) + arena_capacity(40 * n);
        let render = 4 * n + n.div_ceil(8);
        assert_eq!(
            with.at(Phase::Index) - bare.at(Phase::Index),
            index_only + render
        );
        assert_eq!(
            with.at(Phase::Blob) - bare.at(Phase::Blob),
            render,
            "the index-only column went back at the end of the postings"
        );
        assert_eq!(
            with.at(Phase::Assemble) - bare.at(Phase::Assemble),
            render,
            "the render column is read by the segment write and stays"
        );
    }

    /// The disk model over the fixture below, at whatever row count the caller wants.
    fn fixture_disk(args: &crate::BuildArgs, n: u64) -> Residency {
        let payloads = payloads_per_item(args);
        let free = crate::pipeline::available_disk(&args.out);
        let (_routes, tail) = plan_routes(args, n, IdShape::dense(n), &payloads, free);
        disk(
            args,
            Corpus {
                n,
                pair_rows: n,
                term_rows: &[n],
                batches: 1,
                bucket_in_ram: true,
            },
            &payloads,
            &tail,
            &crate::ids::IdSpace::Integer,
        )
    }

    /// The corpus shape [`fixture_disk`] models, for a caller that wants it beside the model.
    fn fixture_corpus(n: u64, term_rows: &[u64]) -> Corpus<'_> {
        Corpus {
            n,
            pair_rows: n,
            term_rows,
            batches: 1,
            bucket_in_ram: true,
        }
    }

    /// **The warning carries the phase and every term in it**, which is the whole of what an
    /// operator can act on: the total says a build may not fit and the breakdown says what to drop.
    ///
    /// Kills a warning that prints the total alone, and one that prints at a free-space figure the
    /// forecast fits inside.
    #[test]
    fn a_forecast_over_the_free_space_warns_with_the_phase_and_its_terms() {
        let n = 2_000;
        let (args, _temp) = fixture(n);
        let disk = fixture_disk(&args, n);
        let term_rows = [n];
        let corpus = fixture_corpus(n, &term_rows);
        let (phase, need) = disk.peak();
        assert!(need > 0, "the fixture has to forecast something");

        assert!(
            forecast_warning(&disk, corpus, need).is_none(),
            "a forecast the free space covers warns about nothing"
        );
        let warning = forecast_warning(&disk, corpus, need - 1)
            .expect("a forecast over the free space warns");
        assert!(
            warning.contains(phase.name()),
            "the warning must name the phase it is about: {warning}"
        );
        let breakdown = disk.describe_phase(phase);
        assert!(
            breakdown.lines().count() > 2,
            "the fixture has to have several terms in its peak phase for this to test anything"
        );
        assert!(
            warning.contains(&breakdown),
            "the warning must carry the phase's per-term breakdown: {warning}"
        );
    }

    /// **An indexed `text` column's token index is in the bundle, and is charged.** Its
    /// `postings.arrow` and `dict.bin` are written at the index phase and released by nothing. A
    /// model that stopped at the column's blob residency reached neither: 176.81 B/item on the
    /// 10⁷ MedCPT sample, 23% of that bundle.
    #[test]
    fn an_indexed_text_columns_token_index_is_a_term_of_the_bundle() {
        let (args, _temp) = fixture(2_000);
        let disk = fixture_disk(&args, 2_000);
        let term = disk
            .terms
            .iter()
            .find(|t| t.what.contains("the text index's postings.arrow"))
            .expect("an indexed text column's token index is a term of its own");
        assert!(term.bytes > 0, "{}", term.what);
        assert!(
            term.phases.holds(Phase::Assemble),
            "the token index is in the bundle to the end of the build"
        );
        // And the blob row the same column keeps, which is a different term over the same
        // characters.
        assert!(
            disk.terms
                .iter()
                .any(|t| t.what.contains("the record blob")),
            "a text column is blob-resident whether or not it is indexed"
        );
    }

    /// **An indexed `keyword` column's dictionary is in the bundle, and is charged.** The scratch
    /// the dictionary pass leaves behind was a term and the dictionary it produces was not.
    #[test]
    fn an_indexed_keyword_columns_dictionary_is_a_term_of_the_bundle() {
        let (args, _temp) = fixture(2_000);
        let disk = fixture_disk(&args, 2_000);
        let term = disk
            .terms
            .iter()
            .find(|t| t.what.contains("': dict.bin"))
            .expect("the finished dictionary is a term of its own");
        assert!(term.phases.holds(Phase::Assemble), "{}", term.what);
        assert!(
            disk.terms
                .iter()
                .any(|t| t.what.contains("the value column's own spool")),
            "values.arrow is assembled from a spool of the same values beside it"
        );
        // **And the entry overhead beside the characters.** The file is front-coded, two varints
        // and a share of a restart offset an entry, so a column of short distinct values that
        // share no prefix is over the characters alone.
        let characters = payload(payloads_per_item(&args)[2], 2_000);
        assert!(
            term.bytes > characters,
            "the charge has to be over the characters alone: {} against {characters}",
            term.bytes
        );
        assert_eq!(
            term.bytes,
            characters + 2_000 * DICT_BYTES_PER_KEY_TENTHS / 10
        );
    }

    /// **The external-id sidecar is four components over two files**, and the locator is one of
    /// them. A term that charged three was 4 B/item short, which is 14 GB at the 3.50×10⁹-row
    /// rung.
    #[test]
    fn the_external_id_sidecar_is_charged_at_the_figure_it_measures() {
        let (mut args, _temp) = fixture(2_000);
        args.mint_external_ids = true;
        // The term is arithmetic over `n` alone, so the model is taken at the row count the
        // measurement was made at rather than the fixture's.
        let n = 1_000_000;
        let disk = fixture_disk(&args, n);
        let term = disk
            .terms
            .iter()
            .find(|t| t.what.contains("the external-id sidecar"))
            .expect("a minting build carries the sidecar as a term of its own");
        // `medcpt-1m` built with `--mint-external-ids` writes 16,251,010 B of
        // `external-ids-0.arrow` and 4,000,000 of `ext-locator.u32` at this `n` — 20,251,010, of
        // which the schema and footer are about a kilobyte (measured,
        // `probes/2026-09-11-disk-forecast/`).
        assert_eq!(term.bytes, 20_250_004);
        assert!(
            term.bytes > 20 * n,
            "the Arrow framing is a quarter byte an item on top of the four buffers"
        );
    }

    /// **A row-major list lane costs its entries, not four bytes an item.** The lane's form is
    /// chosen from the memberships the build resolves, so a level the declaration leaves open is
    /// charged the list form: an offset a row and an ordinal an entry. A layer pinned `rows`
    /// writes no lane at all.
    #[test]
    fn a_row_column_lane_is_charged_its_entries_where_the_form_is_open() {
        use tessera_types::layer::ServingLayout;
        let (mut args, _temp) = fixture(2_000);
        let n = 2_000;
        let entries = member_entries_by_layer(&args);
        assert_eq!(
            entries,
            vec![n],
            "the fixture's member file names every item"
        );

        // One entry an entity over one level: every level can be a label lane.
        assert_eq!(
            row_column_bytes(&args, n, &entries),
            4 * n,
            "a label lane is one ordinal a row and no offset table"
        );
        // Three an entity over the same one level: at least one level overlaps.
        assert_eq!(
            row_column_bytes(&args, n, &[3 * n]),
            4 * n + 4 * 3 * n,
            "a list lane is an offset a row and an ordinal an entry"
        );

        args.layers[0].layout = Some(ServingLayout::RowMajorList);
        assert_eq!(
            row_column_bytes(&args, n, &entries),
            4 * n + 4 * n,
            "a pin settles the form whatever the entry count says"
        );

        args.layers[0].layout = Some(ServingLayout::ArtifactMajor);
        assert_eq!(
            row_column_bytes(&args, n, &[3 * n]),
            0,
            "a layer pinned artifact-major writes no lane"
        );
    }

    /// **A layer on a group draws on every view of it, and is charged every one.** `views` holds
    /// the names the declaration carries, and one of them can be a group
    /// (`Config::expand_layer_views`), so counting the names charges a layer on a ten-view group
    /// one view's lanes.
    ///
    /// A name resolving to no view of this build is charged every view rather than none, which is
    /// the direction a forecast can be wrong in without ending a build at hour three.
    #[test]
    fn a_layer_on_a_group_is_charged_every_view_of_it() {
        let n = 2_000;
        let (mut args, _temp) = fixture(n);
        let view = args.views[0].clone();
        args.views = ["g:a", "g:b", "g:c"]
            .iter()
            .map(|id| crate::ViewArgs {
                view_id: (*id).into(),
                ..view.clone()
            })
            .collect();

        args.layers[0].views = vec!["g".into()];
        assert_eq!(
            layer_views(&args, &args.layers[0]),
            3,
            "a group name draws the layer on every view of the group"
        );
        assert_eq!(
            row_column_bytes(&args, n, &[n]),
            4 * n * 3,
            "one label lane per view of the group"
        );

        args.layers[0].views = vec!["g:a".into(), "g:b".into()];
        assert_eq!(
            layer_views(&args, &args.layers[0]),
            2,
            "two views of the group named one at a time are two views"
        );

        args.layers[0].views = vec!["s0".into()];
        assert_eq!(
            layer_views(&args, &args.layers[0]),
            3,
            "a name matching no view of this build is charged every view, not none"
        );
    }

    /// **`postings.arrow` is a record a term, not four bytes a pair.** A relation of singleton
    /// terms costs thirteen bytes a pair — eight of Arrow offset, a tag and four of entity — where
    /// `4 × pairs` charges four.
    #[test]
    fn the_postings_are_charged_per_term_and_a_singleton_term_costs_thirteen() {
        let n = 1_000_000;
        let singletons = vec![1u64; 1_000];
        assert_eq!(
            postings_bytes(&singletons, n),
            8 * 1_001 + 1_000 * (1 + 4),
            "the offset table and one tagged four-byte entity a term"
        );
        // And a term over the whole entity space is a bitset rather than four bytes a member.
        let whole = postings_bytes(&[n], n);
        assert!(
            whole < 4 * n,
            "a term of 10^6 entities is 8 KiB a container, not 4 B a member: {whole}"
        );
    }

    /// The description leads with the largest **charged** term, because that is the one the
    /// operator is being refused for and the one they can act on.
    #[test]
    fn the_breakdown_leads_with_the_term_worth_acting_on() {
        let described = campaign_residency(50_000_000, 200).describe();
        let first = described
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default();
        assert!(
            first.contains("member entr"),
            "the member tables are the largest charged term at this schema; got {first}"
        );
        assert!(!first.contains("(mapped)"), "got {first}");
    }

    /// **The measurement the model is checked against.** Writes a corpus of `N` items with a text
    /// column and a member table, builds it with an observer, and prints each stage's peak RSS
    /// beside the model's prediction.
    ///
    /// `#[ignore]`d because it writes and builds a real corpus — minutes and gigabytes, which is
    /// not a gate's business. Run it by name when the model's constants are in question:
    ///
    /// ```text
    /// cargo test -p tessera-build --release residency::tests::the_model -- --ignored --nocapture
    /// ```
    /// A corpus of `n` items with one `u32` and one `text` column, and a flat layer whose member
    /// table names every item — the smallest fixture that has each term of the model in it.
    ///
    /// Returns the args and the temp dir, which the caller must hold: the inputs live in it.
    fn fixture(n: u64) -> (crate::BuildArgs, tempfile::TempDir) {
        use arrow::array::{ArrayRef, Float64Array, StringArray, UInt32Array, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;

        const MEMBERS_PER_ARTIFACT: u64 = 64;
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path();
        let write = |path: &std::path::Path, schema: Arc<ArrowSchema>, columns: Vec<ArrayRef>| {
            let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
            let mut writer =
                ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        };

        let points = dir.join("points.parquet");
        let ids: Vec<u64> = (0..n).collect();
        let blurbs: Vec<String> = ids
            .iter()
            .map(|e| format!("item {e} of the residency fixture, prose enough to have a payload"))
            .collect();
        write(
            &points,
            Arc::new(ArrowSchema::new(vec![
                Field::new("entity_id", DataType::UInt64, false),
                Field::new("x", DataType::Float64, false),
                Field::new("y", DataType::Float64, false),
                Field::new("weight", DataType::UInt32, false),
                Field::new("blurb", DataType::Utf8, false),
                Field::new("code", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(UInt64Array::from(ids.clone())) as ArrayRef,
                Arc::new(Float64Array::from(
                    ids.iter().map(|e| (e % 1000) as f64).collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    ids.iter().map(|e| (e % 997) as f64).collect::<Vec<_>>(),
                )),
                Arc::new(UInt32Array::from(
                    ids.iter().map(|e| (e % 64) as u32).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(blurbs)),
                Arc::new(StringArray::from(
                    ids.iter().map(|e| format!("k{e:08}")).collect::<Vec<_>>(),
                )),
            ],
        );

        let pairs = dir.join("pairs.parquet");
        write(
            &pairs,
            Arc::new(ArrowSchema::new(vec![
                Field::new("entity_id", DataType::UInt64, false),
                Field::new("term_id", DataType::UInt32, false),
            ])),
            vec![
                Arc::new(UInt64Array::from(ids.clone())) as ArrayRef,
                Arc::new(UInt32Array::from(
                    ids.iter().map(|e| (e % 512) as u32).collect::<Vec<_>>(),
                )),
            ],
        );

        let artifacts = dir.join("artifacts.parquet");
        let keys: Vec<String> = (0..n.div_ceil(MEMBERS_PER_ARTIFACT))
            .map(|a| a.to_string())
            .collect();
        write(
            &artifacts,
            Arc::new(ArrowSchema::new(vec![Field::new(
                "key",
                DataType::Utf8,
                false,
            )])),
            vec![Arc::new(StringArray::from(keys)) as ArrayRef],
        );
        let members = dir.join("members.parquet");
        write(
            &members,
            Arc::new(ArrowSchema::new(vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("entity", DataType::UInt64, false),
            ])),
            vec![
                Arc::new(StringArray::from(
                    ids.iter()
                        .map(|e| (e / MEMBERS_PER_ARTIFACT).to_string())
                        .collect::<Vec<_>>(),
                )) as ArrayRef,
                Arc::new(UInt64Array::from(ids.clone())),
            ],
        );

        let config = format!(
            r#"
[sources]
points = "{points}"
pairs = "{pairs}"
artifacts = "{artifacts}"
members = "{members}"

[defaults]
source = "points"

[[view]]
name = "s0"
extent = {{ min = 0.0, max = 1024.0 }}
point_visibility = {{ source = "pairs", default = "public" }}

[[attribute]]
name = "weight"
type = "u32"
render = true

[[attribute]]
name = "blurb"
type = "text"
index = true
analyser = "unicode"

[[attribute]]
name = "code"
type = "keyword"
index = true

[[layer]]
name = "fixture/flat"
source = "artifacts"
views = ["s0"]
membership = "enumerated"
hierarchy = {{ kind = "flat" }}
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"

  [layer.members]
  source = "members"
"#,
            points = "points.parquet",
            pairs = "pairs.parquet",
            artifacts = "artifacts.parquet",
            members = "members.parquet",
        );
        let config_path = dir.join("corpus.toml");
        std::fs::write(&config_path, &config).unwrap();
        let parsed = crate::config::Config::parse(&config_path, &Default::default()).unwrap();
        let args = crate::BuildArgs {
            views: vec![crate::ViewArgs {
                visibility: None,
                view_id: "s0".into(),
                projection: parsed.views[0].projection,
                extent: tessera_spatial::Bounds {
                    x_min: 0.0,
                    x_max: 1024.0,
                    y_min: 0.0,
                    y_max: 1024.0,
                },
                points: points.clone(),
                point_fields: parsed.views[0].fields.clone(),
                select: None,
                access: crate::config::AccessInput::relation(pairs.clone()),
            }],
            anchor: 0,
            groups: Vec::new(),
            scoped_attributes: Vec::new(),
            attribute_sources: parsed.attribute_sources.clone(),
            out: dir.join("bundle"),
            limit: None,
            identity_key: tessera_types::IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f")
                .unwrap(),
            identity_key_hex: "000102030405060708090a0b0c0d0e0f".into(),
            idset: 1,
            shard_id: 0,
            layers: parsed.layers.clone(),
            layer_inputs: parsed.layer_sources.clone(),
            scoped_layers: Default::default(),
            mint_external_ids: false,
            emit_oracle_pairs: false,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema: parsed.schema.clone(),
        };

        (args, temp)
    }

    /// **The refusal, before the first pass.** A budget under the tail's own floor stops the build
    /// at the plan rather than at the OOM killer, and the message names the flag and the terms.
    #[test]
    fn a_budget_under_the_tail_refuses_before_the_build_starts() {
        let (mut args, _temp) = fixture(20_000);
        args.memory_budget = Some(32 << 20);
        let error = crate::build(&args).expect_err("a 32 MiB budget cannot hold this tail");
        let message = error.to_string();
        assert!(
            message.contains("entity-order stages"),
            "the refusal should name the stages that do not batch; got {message}"
        );
        assert!(
            message.contains("--memory-budget"),
            "the refusal should name the flag that moves the bound; got {message}"
        );
        // Refused at the plan, which is before the pairs relation is packed and long before a
        // segment exists: the whole point is not paying for the work first.
        let mut written = Vec::new();
        let mut stack = vec![args.out.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    written.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        assert!(
            !written
                .iter()
                .any(|f| f == "columns.arrow" || f == "postings.arrow"),
            "the refusal should land before any segment is written; found {written:?}"
        );
        // And the partial prefix goes back, so a retry has the disk the first attempt took. The
        // dictionary is written under it before the plan runs, so there is something to sweep.
        assert!(
            !args.out.join(crate::PREFIX).exists(),
            "a failed build takes its partial bundle with it; found {written:?}"
        );
    }

    /// And the same build fits under a budget that covers it, so the pre-flight is a bound and not
    /// a blanket — and it leaves nothing behind, which is worth an assertion now that the columns
    /// are files in that directory rather than heap (`column.rs`).
    #[test]
    fn the_same_build_fits_under_a_budget_that_covers_the_tail() {
        let (mut args, _temp) = fixture(20_000);
        args.memory_budget = Some(2 << 30);
        crate::build(&args).expect("2 GiB covers a twenty-thousand-item tail");
        assert!(
            !args.out.join(".build-tmp").exists(),
            "a successful build takes its scratch directory with it"
        );
    }

    /// **A build into a directory that already holds a bundle refuses, and the bundle stands.**
    ///
    /// The argument check runs before the sweep (`crate::pipeline::build`) for this case alone:
    /// `<out>/CURRENT` existing is the one refusal that fires while `<out>/v00000` is the
    /// *published* prefix, so a sweep reaching it is asking to delete a bundle readers can
    /// resolve. It refuses, but the caller is then told the tool tried.
    #[test]
    fn a_build_into_a_published_bundle_root_refuses_and_leaves_it_whole() {
        let (args, _temp) = fixture(2_000);
        crate::build(&args).expect("the first build publishes a bundle");
        let current = std::fs::read(args.out.join("CURRENT")).expect("CURRENT was published");
        let prefix = args.out.join(crate::PREFIX);
        let before: u64 = walk_bytes(&prefix);
        assert!(before > 0, "the published prefix has to hold something");

        let error = crate::build(&args).expect_err("a second build into the same --out refuses");
        assert!(
            error.to_string().contains("already contains a bundle"),
            "the refusal is the argument check's: {error}"
        );
        assert_eq!(
            std::fs::read(args.out.join("CURRENT")).expect("CURRENT still stands"),
            current,
            "the published pointer must be untouched"
        );
        assert_eq!(
            walk_bytes(&prefix),
            before,
            "the published prefix must be untouched"
        );
    }

    /// Every byte under a directory, recursively — what a published prefix weighs.
    fn walk_bytes(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .map(|entry| match entry.path().is_dir() {
                true => walk_bytes(&entry.path()),
                false => entry.metadata().map(|m| m.len()).unwrap_or(0),
            })
            .sum()
    }

    #[test]
    #[ignore = "writes and builds a real corpus to check the model's constants against a peak"]
    fn the_model_is_checked_against_an_observed_peak() {
        const N: u64 = 1_000_000;

        struct Trace;
        impl crate::observer::BuildObserver for Trace {
            fn stage_end(
                &self,
                stage: crate::observer::BuildStage,
                elapsed: std::time::Duration,
                rows: u64,
                peak_rss_kib: u64,
            ) {
                println!(
                    "{:>16}  {:>8.2}s  rows={rows:<12} peak={:>6} MiB",
                    stage.name(),
                    elapsed.as_secs_f64(),
                    peak_rss_kib / 1024
                );
            }
        }

        let (args, _temp) = fixture(N);
        let free = crate::pipeline::available_disk(&args.out);
        let (_routes, model) =
            plan_routes(&args, N, IdShape::dense(N), &payloads_per_item(&args), free);
        println!("model: {} MiB{}", model.total() >> 20, model.describe());
        crate::build_observed(&args, &Trace).unwrap();
        println!(
            "observed build peak: {} MiB",
            crate::observer::peak_rss_kib() / 1024
        );
    }

    /// **The headline of the entity-order model: what anonymous memory grows with the corpus is
    /// named, and it is four terms.**
    ///
    /// Every term the model charges against the machine is a constant or one publication batch — a
    /// share of the budget — with four exceptions, and this test subtracts them rather than
    /// pretending they are not there:
    ///
    /// - arrow's all-ones validity bitmaps during the `columns.arrow` layout pass, `n / 8` bytes a
    ///   column alive together;
    /// - a spilled string column's duplicate map, `n / 4` bytes a column, held from the postings
    ///   stage to the end of the record blob's merge;
    /// - the artifact pass's partition bucket, which holds one record per **member entry** in its
    ///   row range and so is bounded by the largest layer's entries over 128 rather than by the
    ///   `u32` row key. Every other partition pushes one record per key and is flat above the key
    ///   type's bound; this one is not, and a layer of many ordinals a row is where it shows;
    /// - the term images, one worker's posting, image and frozen buffer each at a bitset container
    ///   per 65,536 values. The projection scratch beside them is bounded by one window of row ids
    ///   and is one of the constants.
    ///
    /// Naming them is the point: the assertions are equalities against exactly these four, so a
    /// term that starts rising with the corpus fails here whether or not anyone remembered to look.
    /// What grows properly is the disk the same model reports beside it.
    #[test]
    fn the_anonymous_total_grows_only_by_the_four_terms_this_names() {
        // The floor a partition's own constant needs, and the budget both row counts here carry
        // more member entries than one publication batch of.
        const BUDGET: u64 = 2 << 30;
        let residency = |n: u64| {
            let columns = [
                column(ScalarType::U8, 0),
                column(ScalarType::U32, 0),
                column(ScalarType::Keyword, 8 * n),
                spilled(ScalarType::Text, 400 * n),
            ];
            super::entity_order_residency(
                n,
                IdShape::dense(n),
                &columns,
                &[],
                3 * n,
                3 * n,
                // One layer, so its entries are all of them: three a row, which is the shape that
                // makes the artifact pass's bucket the entry-bounded term it is.
                3 * n,
                BUDGET,
            )
        };
        // Arrow's validity bitmaps: `tessera_id`, `residual` and the two fixed-width declared
        // columns, at one bit a row each. The two that carry characters are not render columns and
        // are not in `columns.arrow`.
        let bitmaps = |n: u64| 4 * n.div_ceil(8);
        // **And the one spilled column's duplicate map**: two whole-column Roaring bitmaps at n/8
        // bytes apiece for the column, and two more for the build, held while that column's map is
        // being built. It is the second term that is a rate in the row count rather than one
        // publication batch, and it is charged for a spilled string column alone — a build with no
        // such column has no such term, which is why the assertions below name it separately
        // rather than folding it into the rate.
        let duplicates = |n: u64| 4 * n.div_ceil(8);
        // **And the term images' three bitmaps**: a posting, an image and a frozen buffer a
        // worker. The projection scratch beside them is bounded by one window and is one of the
        // constants, so it appears in the second assertion and not the first.
        let workers = crate::term_images_pass::derive_threads() as u64;
        let images = |n: u64| workers * term_image_bitmap_bytes(n);
        let image_scratch =
            |n: u64| workers * tessera_store::permutation::project_scratch_bound(n, n).total();
        let small = residency(10_000_000);
        let large = residency(100_000_000);
        // **The rate terms are equal net of the bitmaps**: one publication batch, whatever the
        // level behind it.
        assert_eq!(
            scaling_total(&small)
                - bitmaps(10_000_000)
                - duplicates(10_000_000)
                - images(10_000_000),
            scaling_total(&large)
                - bitmaps(100_000_000)
                - duplicates(100_000_000)
                - images(100_000_000),
            "the anonymous rate is one publication batch, arrow's validity bitmaps, the spilled \
             column's duplicate map and the term images' window:\nat 10⁷{}\nat 10⁸{}",
            small.describe(),
            large.describe()
        );
        // **And the constants are bounded by the key type, not by the corpus** — every one but the
        // artifact pass's. A partition's bucket is `n / 128` records and never more than 2³²/128,
        // so the total rises to that bound and is flat above it; the pass's bucket counts member
        // entries rather than rows, so it goes on rising with them. The two row counts here
        // straddle 2³², and above it the anonymous total moves by the validity bitmaps and that one
        // bucket.
        let pass_bucket = |n: u64| 16 * (3 * n / crate::spill::PARTITION_BUCKETS as u64);
        let at_bound = residency(1u64 << 32);
        let beyond = residency(1u64 << 34);
        assert_eq!(
            beyond.total() - at_bound.total(),
            (bitmaps(1u64 << 34) - bitmaps(1u64 << 32))
                + (duplicates(1u64 << 34) - duplicates(1u64 << 32))
                + (pass_bucket(1u64 << 34) - pass_bucket(1u64 << 32))
                + (images(1u64 << 34) - images(1u64 << 32))
                + (image_scratch(1u64 << 34) - image_scratch(1u64 << 32)),
            "above the key type's bound the anonymous total moves by the validity bitmaps, the \
             spilled column's duplicate map, the artifact pass's bucket and the term images' \
             bitmaps and scratch, and nothing else:\nat 2³²{}\nat 2³⁴{}",
            at_bound.describe(),
            beyond.describe()
        );
        assert!(
            small.total() <= at_bound.total() && large.total() <= at_bound.total(),
            "no corpus costs more than the bound"
        );
        assert!(
            large.peak().1 > small.peak().1,
            "the disk the same model reports does grow with the corpus"
        );
    }
}
