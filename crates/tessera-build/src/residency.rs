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
//! The segment's **row-order** tail moved the same way and at the same time
//! ([`crate::pipeline::permute_attribute_tail`]): eight render columns at 7.4×10⁷ rows were ~2.4 GB
//! of `Vec`, built by `push` immediately after the entity-order columns stopped being heap. ⊘ It
//! was never a term of this model in either form — it lives in the segment write, not in the
//! entity-order window this module covers — so what moved is its cost and not its accounting. What
//! it is now is a mapped file per render column, priced by [`render_tail_bytes`] into the disk
//! pre-flight's assembly phase.
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
//! So the bound is used the way a lower bound can be: **over budget refuses**, because a lower bound
//! that already exceeds the budget settles it, and the band below refuses nothing and prints the
//! numbers instead. What this buys is the difference between *refused in the first second with the
//! arithmetic printed* and *killed at hour two with nothing written* — and it does not claim to
//! catch every build that will not fit. At the campaign's own corner it catches the headline one:
//! 2.5×10⁸ points over the generator's declaration models at tens of gigabytes against the 12 GB
//! budget those runs passed, where 10⁸ under an auto-derived budget still slips through.
//!
//! ⊘ **The disk figure is a ceiling, and measured at 1.2 to 1.7 times the real peak** — the other
//! way round, because it is compared against free space rather than a flag and an under-read is an
//! ENOSPC at hour three. It was a lower bound until 2026-09-10, at 0.51 to 0.59 of the peak at four
//! row counts from 16.3×10⁶ to 125.8×10⁶ GBIF occurrences, and rung 6 died inside the difference
//! (`probes/2026-09-10-build-disk/`). What is left of the margin is three stated ceilings, named at
//! their terms: the postings and the oracle's pairs at 4 B a pair, the record blob at half its
//! columns' characters, and a published member entry at 3 B.

use tessera_spatial::ScalarType;

/// The unenumerated transients: decode buffers, a stage's scratch, the allocator's slack. The
/// batch loop's own model carries a constant of the same size and for the same reason.
pub(crate) const SLACK: u64 = 64 << 20;

/// What a string value costs outside its characters: the entity-indexed arena offset in
/// [`crate::column::EntityColumn`], plus the arena record's own header.
///
/// Eight bytes of offset and four of record header — the length, and the entity too where the
/// column is one read in arena order (`column.rs`, [`crate::column`]'s `RECORD_HEADER_INDEXED`).
/// The wider header is charged for a `text` column, which is the only family walked that way. The
/// characters ride in the arena and are counted as its payload; the `String` header this replaced
/// was 24 bytes per entity, paid before a character was stored.
fn arena_offset(ty: ScalarType) -> u64 {
    match ty {
        ScalarType::Text => 8 + 8,
        _ => 8 + 4,
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
    /// The attribute join and the layer publication — **the model's largest phase on every corpus
    /// measured**, and within 3% of the index phase beside it, which is where the measurement puts
    /// the peak at 125.8×10⁶ items. Every declared column filling, the join's staging buffer, the
    /// source ids, and the member spill.
    Join,
    /// The filter postings, the keyword dictionaries and the text index: the columns full, their
    /// dictionaries' scratch beside them, and the value columns being written.
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
    /// Whether this column is the one a text index is built over — which costs the build a second
    /// set of files beside the column itself, and costs it them at the same time.
    pub text_index: bool,
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
/// that ratio is one corpus's prose at one operating point, and this figure refuses a build
/// rather than warning about one. A keyword column compresses harder still where its values
/// repeat: GBIF's `scientificname` spilled 1.63 GB of extents over 3.93 GB of characters, 0.42×,
/// and its blocks alone 0.27× (measured, `probes/2026-09-10-blob-resident-strings/`). ⊘ Modelled
/// for the extents, measured for the blob.
const EXTENT_SHARE: u64 = 2;

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

/// What one layer member **entry** — one `(artifact, source)` pair — costs the **machine**: about
/// 4 bytes as Roaring, the store's own decoded copy, at the ~2 bytes an array container spends on a
/// scattered member and less on a dense one, and the level being published beside it, whose
/// incoming bitmaps and durable record bytes are the same membership twice more.
///
/// **Charged over one level's entries and not the layer's**, because [`crate::layers::publish`]
/// walks the levels and drops each one's bitmaps when its record is written. A level draws at most
/// one artifact per member row, so the file's row count is a ceiling on any one level's entries and
/// is what this is charged over — where the disk terms below are charged over every entry, all of
/// which are on the disk at once.
///
/// It was 12 until 2026-08-30, the other 8 being the plan's `Vec<u64>` of source ids: one vector
/// per artifact, every one of them live from the first row of the first member source until the
/// last level was published. Those pairs go to disk now ([`SPILLED_BYTES_PER_MEMBER_ENTRY`]), so the
/// plan holds a spill budget rather than the corpus and the term that is left is the published
/// memberships alone.
///
/// **Only the store's copy was corpus-wide, and it is a mapping now** — see
/// [`MAPPED_BYTES_PER_MEMBER_ENTRY`]. The store used to carry one heap Roaring bitmap per artifact
/// from the layers stage to the artifact pass four stages later: **+1.2 GB of anonymous memory at
/// the 10⁷ MedCPT sample**, 2.7 B per closed member row, measured across the build's peak
/// (`probes/2026-09-02-mapped-memberships/README.md`). `layers.rs` reads each membership back
/// through the extent it has just written, so what is left here is the publication's own window and
/// this constant no longer describes anything the build holds to the end.
///
/// ⊘ **The Roaring figure is the scattered case and is not measured per build.** A dense membership
/// costs an eighth of it; the model takes the expensive one, because the refusal it feeds is meant
/// to be wrong in the direction that costs a rerun rather than a kill. It is loose in one more
/// direction since the packing was streamed: the constant charges the corpus for terms that are a
/// level's, and it is left at 4 rather than lowered because a term that errs high refuses a build
/// that would have fitted, where one that errs low is the kill this module exists to pre-empt.
const BYTES_PER_MEMBER_ENTRY: u64 = 4;

/// What one member entry costs the **disk**, and the process's page cache, as the packed membership
/// extent the store then reads through: 3 bytes, an array container's own width and a little.
///
/// ⊘ **Measured at 2.24 and charged at 3.** GBIF's three taxonomy levels over 125,789,091
/// occurrences write 843.8 MB of extent for 377,367,273 declared key values
/// (`probes/2026-09-10-build-disk/`); the 10⁷ MedCPT sample's two extents are 543,884,239 bytes
/// over 471,778,374 entries — **1.15 B an entry**, because a descriptor whose members are dense in
/// entity space comes out as a run or a bitset container rather than an array. The model charges
/// above the larger figure for the reason the constant above does: a term that errs high costs a
/// rerun and one that errs low is the ENOSPC this pre-flight exists to pre-empt.
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
/// ⊘ **Measured at 2.9 and charged at 4.** GBIF's taxonomy spills at most 1,084.6 MB of runs and
/// table together over 377,367,273 declared key values, at four row counts from 16.3×10⁶ to
/// 125.8×10⁶ and within 4% of each other (`probes/2026-09-10-build-disk/`). GeoNames' two member
/// files carry 68.4×10⁶ pairs and spilled 73 MiB of runs beside a 68 MiB table — 2.2 B an entry.
const SPILLED_BYTES_PER_MEMBER_ENTRY: u64 = 4;

/// What the segment's **row-order** tail costs on disk: one fixed-width slot per row per render
/// column, in `.build-tmp/`, for the length of the segment write.
///
/// Not a term of [`entity_order_residency`], because it is not in that window: the tail is built
/// after the release that ends it, and every column it covers is one the release *kept*. It is the
/// assembly phase's, beside `columns.arrow` and the postings spool.
///
/// A `bool` is counted twice over, at a byte a row and again at a bit: the lane fills a byte per
/// row and packs it into Arrow's bit layout on the way out, and both mappings stand while it does.
/// Every other render column is its declared width and nothing else — a string one cannot be here,
/// `render` being refused for the whole family at the declaration.
pub(crate) fn render_tail_bytes(schema: &crate::config::Schema, n: u64) -> u64 {
    schema
        .attributes
        .iter()
        .filter(|a| a.render)
        .map(|a| match a.ty {
            ScalarType::Bool => n.saturating_add(n.div_ceil(8)),
            ty => fixed_width(ty).saturating_mul(n),
        })
        .sum()
}

/// The residency of everything the batch loop's model does not cover.
///
/// `member_entries` is every `(artifact, source)` pair the layers' member sources declare and
/// `level_entries` a ceiling on any one level's, those being the two windows a member entry is held
/// in: the spill holds every pair at once, the publication one level's. `n` is the item count.
pub(crate) fn entity_order_residency(
    n: u64,
    columns: &[ColumnCost],
    member_entries: u64,
    level_entries: u64,
) -> Residency {
    let publication_bytes = level_entries.saturating_mul(BYTES_PER_MEMBER_ENTRY);
    let mut terms = vec![
        // **File-backed since 2026-09-10**, and so charged to the disk rather than to memory. The
        // ids are read sequentially by every pass but one — the join's merge sweep, the
        // external-id write, the ordinal walks — and the exception is `layers::publish`'s binary
        // search on the sparse path, which is the random-access case `MappedArray` was written
        // for. 8 B/item is 26.0 GiB at the GBIF rung. They are released at the layer publication,
        // so they are on the disk for every phase of the pre-flight but the assembly.
        Term {
            what: "the sorted source ids, 8 B/item, in .build-tmp/ (released at the layer \
                   publication)"
                .into(),
            bytes: 8 * n,
            mapped: true,
            phases: Phases::SPILL.and(Phases::BANDS).and(Phases::JOIN),
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
        },
    ];
    for (index, column) in columns.iter().enumerate() {
        let width = fixed_width(column.ty);
        let presence = n.div_ceil(8);
        // **A spilled column has no arena and no offset array.** What it has instead is one
        // record-blob extent per join chunk, holding the same characters compressed
        // ([`EXTENT_SHARE`]); the presence bits are all that is left of the column itself.
        let spilled = column.extents;
        let bytes = if spilled {
            presence.saturating_add(column.payload_bytes / EXTENT_SHARE)
        } else if column.payload_bytes > 0 {
            // 8 bytes of entity-indexed offset and the presence bit, plus the arena the characters
            // and their record headers fill.
            8u64.saturating_mul(n)
                .saturating_add(presence)
                .saturating_add(arena_capacity(
                    column
                        .payload_bytes
                        .saturating_add((width - 8) * n),
                ))
        } else {
            width
                .saturating_mul(n)
                .saturating_add(presence)
        };
        let ty = column.ty.arrow_type_name();
        terms.push(Term {
            what: if spilled {
                format!(
                    "declared column {index} ({ty}): {} MiB of extents in .build-tmp/, \
                     modelled at half the source's {} MiB of characters",
                    (column.payload_bytes / EXTENT_SHARE) >> 20,
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
        });
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
            });
        }
    }
    if member_entries > 0 {
        terms.push(Term {
            what: format!(
                "{level_entries} member entr(ies) in the largest level at \
                 {BYTES_PER_MEMBER_ENTRY} B — the publication's own Roaring, while the level it is \
                 publishing is in flight"
            ),
            bytes: publication_bytes,
            mapped: false,
            phases: Phases::JOIN,
        });
        terms.push(Term {
            what: format!(
                "the published memberships the store reads back through the packed extent, at \
                 {MAPPED_BYTES_PER_MEMBER_ENTRY} B a member entry"
            ),
            bytes: member_entries.saturating_mul(MAPPED_BYTES_PER_MEMBER_ENTRY),
            mapped: true,
            // The bundle's own extents: written at the publication and never released.
            phases: Phases::JOIN.onwards(),
        });
        terms.push(Term {
            what: format!(
                "the member spill's runs and the table they merge into, at \
                 {SPILLED_BYTES_PER_MEMBER_ENTRY} B a member entry, in .build-tmp/"
            ),
            bytes: member_entries.saturating_mul(SPILLED_BYTES_PER_MEMBER_ENTRY),
            mapped: true,
            phases: Phases::JOIN,
        });
    }
    terms.push(Term {
        what: "slack for decode buffers, stage scratch and the allocator".into(),
        bytes: SLACK,
        mapped: false,
        phases: Phases::JOIN,
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
) -> (Vec<ColumnCost>, u64, u64) {
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
            // The same test the emit itself makes, called rather than restated: a text column
            // earns an index exactly where it is owed postings.
            text_index: attribute.ty == ScalarType::Text
                && crate::pipeline::postings_are_owed(&args.schema, attribute),
            phases: if attribute.render {
                Phases::JOIN.onwards()
            } else if crate::pipeline::blob_resident(&args.schema, attribute) {
                Phases::JOIN.and(Phases::INDEX).and(Phases::BLOB)
            } else {
                Phases::JOIN.and(Phases::INDEX)
            },
        })
        .collect();
    // **Two denominators over the same member sources**, because a member entry is held in two
    // windows of different sizes. Every entry is on the disk at once, as the spill's runs and the
    // table they merge into: that is the file's key values. One *level's* are in memory at once, as
    // the publication's Roaring, and a level draws at most one artifact per member row: that is the
    // file's rows, which is a ceiling on any one level whatever the shape of its key lists.
    let mut entries = 0u64;
    let mut level_entries = 0u64;
    for layer in &args.layer_inputs {
        let Some(members) = layer.members.as_ref() else {
            continue;
        };
        let (declared, rows) = member_entries(members);
        entries = entries.saturating_add(declared);
        level_entries = level_entries.saturating_add(rows);
    }
    (columns, entries, level_entries)
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
    payloads: &[f64],
    free: Option<u64>,
) -> (crate::pipeline::ColumnRoutes, Residency) {
    let spilled = crate::pipeline::ColumnRoutes::every_available(&args.schema);
    let (columns, entries, level_entries) = model_inputs(args, n, payloads, &spilled);
    choose_routes(&args.schema, n, columns, entries, level_entries, free)
}

/// [`plan_routes`] unless the caller named the route ([`crate::ExtentRoute`]), in which case the
/// named one is taken and the model is rebuilt over it — so the disk forecast a forced build
/// prints describes the build it is about to run.
pub(crate) fn routes_for(
    args: &crate::BuildArgs,
    n: u64,
    payloads: &[f64],
    free: Option<u64>,
    route: crate::ExtentRoute,
) -> (crate::pipeline::ColumnRoutes, Residency) {
    let forced = match route {
        crate::ExtentRoute::Derived => return plan_routes(args, n, payloads, free),
        crate::ExtentRoute::Arena => crate::pipeline::ColumnRoutes::forced_only(&args.schema),
        crate::ExtentRoute::Extents => {
            crate::pipeline::ColumnRoutes::every_available(&args.schema)
        }
    };
    let (columns, entries, level_entries) = model_inputs(args, n, payloads, &forced);
    let tail = entity_order_residency(n, &columns, entries, level_entries);
    (forced, tail)
}

/// [`plan_routes`] over the model's inputs rather than the build's, so the rule can be tested at a
/// schema and a free-space figure without a corpus behind them. `columns` arrives with every
/// available column spilled.
fn choose_routes(
    schema: &crate::config::Schema,
    n: u64,
    mut columns: Vec<ColumnCost>,
    entries: u64,
    level_entries: u64,
    free: Option<u64>,
) -> (crate::pipeline::ColumnRoutes, Residency) {
    let mut routes = crate::pipeline::ColumnRoutes::every_available(schema);
    let ceiling = free.unwrap_or(0) / ROUTE_HEADROOM;
    for (index, attribute) in schema.attributes.iter().enumerate() {
        if !routes.takes_extents(index) || crate::pipeline::extents_are_forced(schema, attribute) {
            continue;
        }
        columns[index].extents = false;
        let candidate = entity_order_residency(n, &columns, entries, level_entries);
        if stage_scratch(&candidate) <= ceiling {
            routes.take_arena(index);
        } else {
            columns[index].extents = true;
        }
    }
    let tail = entity_order_residency(n, &columns, entries, level_entries);
    (routes, tail)
}

/// **What the entity-order stages have on the disk at once**: the largest phase's mapped terms.
///
/// The anonymous terms are not in it. They are the publication's Roaring and one slack constant,
/// neither of which a route moves, and they are memory rather than space.
pub(crate) fn stage_scratch(residency: &Residency) -> u64 {
    residency.peak().1
}

/// **What the whole build asks the disk for**, phase by phase, so the pre-flight refuses on the
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
pub(crate) fn disk(
    args: &crate::BuildArgs,
    n: u64,
    pair_rows: u64,
    batches: u64,
    bucket_in_ram: bool,
    payloads: &[f64],
    tail: &Residency,
) -> Residency {
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
    // ids under it do not move (I9). It is written scattered by the pairs pack and read scattered
    // by the assignment walk, so it stands from the pairs pass to that walk and no longer — the
    // anchor geometry's window exactly.
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
        format!(
            "each view's geometry by ordinal, 8 B/item over {views} view(s), in .build-tmp/"
        ),
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
    push(
        "the row space's geometry in entity order, 8 B/item, in .build-tmp/ (one view at a time)"
            .into(),
        8 * n,
        Phases::ASSEMBLE,
    );
    push(
        "the row-order render tail, one mapped file per render column, in .build-tmp/".into(),
        render_tail_bytes(&args.schema, n),
        Phases::ASSEMBLE,
    );

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
        "postings.arrow, at a ceiling of 4 B/pair".into(),
        4 * p,
        Phases::BANDS.onwards(),
    );
    if args.emit_oracle_pairs {
        push(
            "pairs.parquet, at a ceiling of 4 B/pair — the test-time oracle's copy of the \
             relation, which --no-oracle-pairs does not write"
                .into(),
            4 * p,
            Phases::BANDS.onwards(),
        );
    }
    if args.mint_external_ids {
        push(
            "the external-id sidecar and its locator, 12 B/item".into(),
            12 * n,
            Phases::BANDS.onwards(),
        );
    }
    let mut blob_payload = 0u64;
    for (index, attribute) in args.schema.attributes.iter().enumerate() {
        if crate::pipeline::blob_resident(&args.schema, attribute) {
            blob_payload = blob_payload.saturating_add(payload(payloads[index], n));
            continue;
        }
        if !crate::pipeline::postings_are_owed(&args.schema, attribute) {
            continue;
        }
        // An indexed keyword column's values file is its dictionary ordinals, four bytes a row;
        // every other family's is its declared width. The presence bitmap beside it is charged at
        // a bit an item, which is a Roaring bitmap's own ceiling.
        let width = match attribute.ty {
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => 4,
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
        if matches!(attribute.ty, ScalarType::Utf8 | ScalarType::Keyword) {
            // The dictionary pass's own scratch, under the column's directory rather than
            // `.build-tmp/`: a `u32` ordinal per row scattered out of the merge, and the sorted
            // runs it merges, both gone by the end of the pass. Measured at 5.3 B/item over
            // 125.8×10⁶ GBIF occurrences against the 8 this charges.
            push(
                format!(
                    "attribute '{}': the keyword dictionary's row ordinals and sorted runs, \
                     4 B/item and a ceiling of half the column's characters",
                    attribute.name
                ),
                4 * n + payload(payloads[index], n) / EXTENT_SHARE,
                Phases::INDEX,
            );
        }
    }
    if blob_payload > 0 {
        push(
            format!(
                "the record blob: {} MiB of directory at 4 B/item, and its blocks modelled at half \
                 the {} MiB of characters its columns carry",
                (4 * n) >> 20,
                blob_payload >> 20
            ),
            4 * n + blob_payload / EXTENT_SHARE,
            Phases::BLOB.onwards(),
        );
    }
    // Per view: `morton.u32`, `permutation.bin`, `row-entity.u32` and the segment's `columns.arrow`
    // — the residual, the `tessera_id` and one slot per render column.
    let render: u64 = args
        .schema
        .attributes
        .iter()
        .filter(|a| a.render)
        .map(|a| fixed_width(a.ty))
        .sum();
    push(
        format!("each view's segment, permutation and row→entity files, over {views} view(s)"),
        (4 + 4 + 4 + 4 + 8 + render) * n * views,
        Phases::ASSEMBLE,
    );
    // The artifact pass's row-column lane, one `u32` ordinal a row per level it draws.
    let levels: u64 = args
        .layers
        .iter()
        .map(|layer| layer.levels.len().max(1) as u64)
        .sum();
    push(
        format!("the artifact pass's row-column lanes, 4 B/item over {levels} level(s) a view"),
        4 * n * views * levels,
        Phases::ASSEMBLE,
    );
    Residency { terms }
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

    /// A column whose characters, if it has any, fill an arena — the shape a string column keeps
    /// while some pass reads it at an entity.
    fn column(ty: ScalarType, payload: u64) -> ColumnCost {
        ColumnCost {
            ty,
            payload_bytes: payload,
            extents: false,
            text_index: false,
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
        entity_order_residency(n, &columns, entries, entries)
    }

    #[test]
    fn the_tail_is_linear_in_the_item_count_and_the_batch_stride_reaches_none_of_it() {
        let small = campaign_residency(10_000_000, 200);
        let large = campaign_residency(50_000_000, 200);
        // Five times the corpus, five times the residency — exactly, net of the one term that is
        // not a function of the corpus at all.
        let ratio = (large.total() - SLACK) as f64 / (small.total() - SLACK) as f64;
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
        let with_layer = entity_order_residency(n, &[], n, n);
        let without = entity_order_residency(n, &[], 0, 0);

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
        assert!(ids.mapped, "the ids are a file, not memory the machine must have");
        assert!(
            with_layer.describe().contains("MiB (mapped)  the sorted source ids"),
            "a refusal has to show the disk the build wants: {}",
            with_layer.describe()
        );
    }

    /// **A declared column is reported and not charged.** The whole of a keyword column — its
    /// per-entity offsets, its presence bits and every character it holds — is a file under
    /// `.build-tmp/`, so it appears in the breakdown at its full size and adds nothing to the
    /// figure `--memory-budget` is compared against.
    #[test]
    fn a_declared_column_is_reported_as_mapped_and_charged_at_nothing() {
        let n = 10_000_000;
        let bare = entity_order_residency(n, &[], 0, 0);
        let with_keyword = entity_order_residency(n, &[column(ScalarType::Keyword, 400 * n)], 0, 0);
        assert_eq!(with_keyword.total(), bare.total());
        let term = with_keyword
            .terms
            .iter()
            .find(|t| t.what.contains("declared column"))
            .expect("the column is a term of its own");
        // The offsets, the presence bits, and the arena the characters and their record headers
        // fill.
        assert_eq!(
            term.bytes,
            8 * n + n.div_ceil(8) + arena_capacity(400 * n + 4 * n)
        );
        assert!(
            with_keyword.describe().contains("(mapped)"),
            "the breakdown must say which terms are files: {}",
            with_keyword.describe()
        );
    }

    /// **A spilled column costs its extents, not an arena.** Its characters are written once as
    /// record-blob blocks under `.build-tmp/`, charged at half the source's characters
    /// ([`EXTENT_SHARE`]); the column itself is presence bits and nothing else. That holds for
    /// every string type the route takes, and what it removes on a `keyword` column is the offset
    /// array as well as the arena — 12 B/item before a character.
    #[test]
    fn a_spilled_column_costs_its_extents_and_not_an_arena() {
        let n = 10_000_000;
        let payload = 400 * n;
        for ty in [ScalarType::Text, ScalarType::Keyword, ScalarType::Utf8] {
            let route = entity_order_residency(n, &[spilled(ty, payload)], 0, 0);
            let term = route
                .terms
                .iter()
                .find(|t| t.what.contains("declared column"))
                .expect("the column is a term of its own");
            assert_eq!(term.bytes, n.div_ceil(8) + payload / 2, "{ty:?}");
            assert!(
                term.what.contains("MiB of extents"),
                "the breakdown must name them: {}",
                term.what
            );
            // The arena route on the same column, for the difference the routing is worth.
            let arena = entity_order_residency(n, &[column(ty, payload)], 0, 0);
            let held = arena
                .terms
                .iter()
                .find(|t| t.what.contains("declared column"))
                .expect("the column is a term of its own");
            assert!(
                held.bytes > term.bytes + 8 * n,
                "{ty:?}: the arena route costs the offsets and the characters whole"
            );
        }
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
            &[column(ScalarType::Keyword, payload)],
            0,
            0,
        ));

        let fits = choose_routes(
            &schema,
            n,
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
        let (routes, _) = choose_routes(&schema, n, columns, 0, 0, None);
        assert!(routes.takes_extents(0) && routes.takes_extents(1));
    }

    /// **`text` spills however much disk there is** (`build-column-extents.md` §2). Its arena is
    /// filled by a second decode of the source in entity order, and that cost is the source
    /// permutation rather than the arena's size, so no amount of space buys it back. An indexed
    /// `keyword` is the other unconditional case and goes the other way: the dictionary writer
    /// reads it at an entity, so it keeps its arena however little space is left.
    #[test]
    fn the_two_unconditional_families_ignore_the_free_space() {
        let n = 1_000_000;
        let payload = 400 * n;
        let schema = route_schema(&[(ScalarType::Text, true), (ScalarType::Keyword, true)]);
        for free in [1 << 20, 1 << 30, 1 << 40] {
            let columns = vec![
                spilled(ScalarType::Text, payload),
                column(ScalarType::Keyword, payload),
            ];
            let (routes, _) = choose_routes(&schema, n, columns, 0, 0, Some(free));
            assert!(routes.takes_extents(0), "text spills with {free} bytes free");
            assert!(
                !routes.takes_extents(1),
                "an indexed keyword keeps its arena with {free} bytes free"
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
            &[
                column(ScalarType::Keyword, payload),
                spilled(ScalarType::Utf8, payload),
            ],
            0,
            0,
        ));
        let both = stage_scratch(&entity_order_residency(
            n,
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
        let (routes, _) = choose_routes(&schema, n, columns, 0, 0, Some(one * ROUTE_HEADROOM));
        assert!(!routes.takes_extents(0), "the first column takes the arena");
        assert!(
            routes.takes_extents(1),
            "the second is charged against the window the first left"
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
            let (routes, _) = choose_routes(&schema, n, gbif(n), 3 * n, n, free);
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
        let extent_bytes = n.div_ceil(8) + 200 * n;
        // The source ids and the ordinal→entity map are files whatever the schema declares, so
        // each figure below is stated against a build declaring no column at all.
        let bare = entity_order_residency(n, &[], 0, 0).at(Phase::Index);

        let without = entity_order_residency(n, &[plain], 0, 0);
        assert_eq!(without.at(Phase::Index) - bare, extent_bytes);

        // The runs are charged the source's characters, being uncompressed where the extents that
        // hold the same prose are not.
        let with = entity_order_residency(n, &[indexed], 0, 0);
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
        let without = entity_order_residency(n, &[], 0, 0);
        let with = entity_order_residency(n, &[], rows, rows);
        assert_eq!(
            with.total() - without.total(),
            rows * BYTES_PER_MEMBER_ENTRY,
            "only the Roaring copies are memory"
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
        let residency = entity_order_residency(n, &[], entries, entries);

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
        let bare = entity_order_residency(n, &[], 0, 0);
        let with = entity_order_residency(n, &[indexed, rendered], 0, 0);

        let index_only = 8 * n + n.div_ceil(8) + arena_capacity(40 * n + 4 * n);
        let render = 4 * n + n.div_ceil(8);
        assert_eq!(with.at(Phase::Index) - bare.at(Phase::Index), index_only + render);
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
        let (_routes, model) = plan_routes(&args, N, &payloads_per_item(&args), free);
        println!("model: {} MiB{}", model.total() >> 20, model.describe());
        crate::build_observed(&args, &Trace).unwrap();
        println!(
            "observed build peak: {} MiB",
            crate::observer::peak_rss_kib() / 1024
        );
    }
}
