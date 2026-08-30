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
//! # What is resident there, and why none of it batches
//!
//! From the attribute pass to the segment write the build holds, all at once:
//!
//! - the **sorted source ids** and the **ordinal→entity map**, 12 bytes an item, because a member
//!   and an attribute row are both named by source id and both have to resolve;
//! - the **published memberships** in the store, as Roaring.
//!
//! **None of it is a batch.** The loop's residency shrinks when the stride does; this does not
//! shrink at all, because a member table is its own size. So the honest answer is not a smaller
//! batch, it is a refusal that names the number — which is what this module computes and
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
//! Every **declared column in entity order** was the other half of this list and the larger half of
//! the campaign's kills: a fixed-width type at its own width, a `text`, `keyword` or `utf8` one at a
//! `String` *per entity* — 24 bytes of header before a character was stored — plus a presence bit
//! each. They are now files under `.build-tmp/`, mapped rather than held ([`crate::column`]), so
//! they cost the machine page cache the kernel may evict and not memory it must have.
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
//! **[`Residency::mapped`] is the other half, and the disk pre-flight is its reader.** Those files
//! are on the disk from the attribute join to the column release, and the text index and the
//! member spill both write their runs into the same window — a stretch the pre-flight's three
//! original phase peaks all end before. Its column phase is this figure, taken from here rather
//! than derived a second time.
//!
//! # What the numbers are, and what they are not
//!
//! Every term below is arithmetic over things known before the first pass: the item count, the
//! declared schema, and two figures read from Parquet footers — a member table's row count and a
//! column's *uncompressed* byte size. No data is read.
//!
//! ⊘ **This is a lower bound, and measured to be about half the real peak.** Transients inside a
//! stage — a Parquet decode buffer, an analyser's scratch, the allocator's own slack — are not
//! enumerated, and [`SLACK`] is one constant standing in for all of them. The one build it has been
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

use tessera_spatial::ScalarType;

/// The unenumerated transients: decode buffers, a stage's scratch, the allocator's slack. The
/// batch loop's own model carries a constant of the same size and for the same reason.
pub(crate) const SLACK: u64 = 64 << 20;

/// What a string value costs in the entity-indexed array of [`crate::column::EntityColumn`]: the
/// arena offset alone. The characters ride in the arena and are counted as its payload; the
/// `String` header this replaced was 24 bytes per entity, paid before a character was stored.
const ARENA_OFFSET: u64 = 8;

/// One named term of the residency, so a refusal prints where the bytes are rather than a total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Term {
    pub what: String,
    pub bytes: u64,
    /// A file under `.build-tmp/` rather than anonymous memory — reported, but not charged against
    /// the memory budget.
    pub mapped: bool,
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

    /// What the build asks the **disk** for: the mapped terms only — the declared columns, their
    /// arenas and the text index's runs, all files under the build's own scratch. The counterpart
    /// of [`Self::total`], and the term the disk pre-flight's column phase is built from
    /// (`pipeline::plan_build`): those files stand from the attribute join to the column release,
    /// which is a window none of the three phases that pre-flight modelled before touches.
    pub fn mapped(&self) -> u64 {
        self.terms
            .iter()
            .filter(|t| t.mapped)
            .map(|t| t.bytes)
            .sum()
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
/// `payload_bytes` is the column's **uncompressed** size in its Parquet source, read from the
/// footer. It is the closest thing to the in-memory string payload that can be had without reading
/// the file, and it is an under-read rather than an over-read: Parquet's uncompressed size is the
/// encoded page size, and a dictionary-encoded column of repeated strings expands when it is
/// materialised into one `String` per entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnCost {
    pub ty: ScalarType,
    pub payload_bytes: u64,
    /// Whether this column is the one a text index is built over — which costs the build a second
    /// set of files beside the column itself, and costs it them at the same time.
    pub text_index: bool,
}

/// Whether one entity's value lives in [`crate::column::EntityColumn`]'s arena — which is what
/// makes the column's characters a term of their own rather than part of its width.
fn is_variable_width(ty: ScalarType) -> bool {
    matches!(
        ty,
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text
    )
}

/// The fixed width one entity's value occupies in [`crate::column::EntityColumn`]'s typed
/// storage. A variable-width type answers [`ARENA_OFFSET`] here and carries its characters in
/// [`ColumnCost::payload_bytes`].
fn fixed_width(ty: ScalarType) -> u64 {
    match ty {
        ScalarType::Bool => 1,
        ScalarType::U8 | ScalarType::I8 => 1,
        ScalarType::U16 | ScalarType::I16 => 2,
        ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 4,
        ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 8,
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => ARENA_OFFSET,
    }
}

/// What one layer member row costs the **machine**: about 4 bytes as Roaring — the store's own
/// decoded copy, at the ~2 bytes an array container spends on a scattered member and less on a
/// dense one, and the level being published beside it, whose incoming bitmaps and durable record
/// bytes are the same membership twice more.
///
/// It was 12 until 2026-08-30, the other 8 being the plan's `Vec<u64>` of source ids: one vector
/// per artifact, every one of them live from the first row of the first member source until the
/// last level was published. Those pairs go to disk now ([`SPILLED_BYTES_PER_MEMBER_ROW`]), so the
/// plan holds a spill budget rather than the corpus and the term that is left is the published
/// memberships alone.
///
/// **Only the store's copy is corpus-wide, and that is what changed later the same day.** The
/// packing used to encode every unpublished level's records into a `Vec<Vec<u8>>` at once and then
/// concatenate each level again — two more copies of every membership in the bundle, both linear in
/// the corpus. `layers.rs` streams a blob at a time into the extent now, so what stands beside the
/// store is one blob and one level's publication rather than the whole corpus's.
///
/// ⊘ **The Roaring figure is the scattered case and is not measured per build.** A dense membership
/// costs an eighth of it; the model takes the expensive one, because the refusal it feeds is meant
/// to be wrong in the direction that costs a rerun rather than a kill. It is loose in one more
/// direction since the packing was streamed: the constant charges the corpus for terms that are now
/// a level's, and it is left at 4 rather than lowered because a term that errs high refuses a build
/// that would have fitted, where one that errs low is the kill this module exists to pre-empt.
const BYTES_PER_MEMBER_ROW: u64 = 4;

/// What one layer member row costs the **disk** while the publication is running: the sorted runs
/// the member spill writes and the merged member table it reads back, both under `.build-tmp/` and
/// both alive at once — the runs are deleted only once the whole table is written.
///
/// Charged as if a pair cost one raw source id in each file. Both are LEB128 delta encodings over
/// ascending values, so the true figure is below that wherever a membership is dense in its id
/// space and at it where the membership is scattered.
///
/// ⊘ **Modelled, not measured per build**, and the one corpus with a figure is GeoNames: its two
/// member files' 26.9×10⁶ Parquet rows carry 68.4×10⁶ `(artifact, source)` pairs, which spilled
/// 73 MiB of runs beside a 68 MiB table — **140 MiB against the 205 MiB this charges**. Loose in
/// the direction the whole module is loose in, and loose the other way in its denominator: a
/// member row's `key` column is a list, so a row is one pair on a flat layer and one per level on
/// a ladder.
const SPILLED_BYTES_PER_MEMBER_ROW: u64 = 8;

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
/// `member_rows` is the total across every layer's member table, and `n` the item count.
pub(crate) fn entity_order_residency(
    n: u64,
    columns: &[ColumnCost],
    member_rows: u64,
) -> Residency {
    let mut terms = vec![
        Term {
            what:
                "the sorted source ids, 8 B/item (input.rs; released after the layer publication)"
                    .into(),
            bytes: 8 * n,
            mapped: false,
        },
        Term {
            what: "the ordinal→entity map, 4 B/item".into(),
            bytes: 4 * n,
            mapped: false,
        },
    ];
    for (index, column) in columns.iter().enumerate() {
        let width = fixed_width(column.ty);
        let presence = n.div_ceil(8);
        let bytes = width
            .saturating_mul(n)
            .saturating_add(presence)
            .saturating_add(column.payload_bytes);
        let ty = column.ty.arrow_type_name();
        terms.push(Term {
            what: if column.payload_bytes > 0 {
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
                bytes,
                mapped: true,
            });
        }
    }
    if member_rows > 0 {
        terms.push(Term {
            what: format!(
                "{member_rows} layer member row(s) at {BYTES_PER_MEMBER_ROW} B — the published \
                 memberships, as Roaring"
            ),
            bytes: member_rows.saturating_mul(BYTES_PER_MEMBER_ROW),
            mapped: false,
        });
        terms.push(Term {
            what: format!(
                "the member spill's runs and the table they merge into, at \
                 {SPILLED_BYTES_PER_MEMBER_ROW} B a member row, in .build-tmp/"
            ),
            bytes: member_rows.saturating_mul(SPILLED_BYTES_PER_MEMBER_ROW),
            mapped: true,
        });
    }
    terms.push(Term {
        what: "slack for decode buffers, stage scratch and the allocator".into(),
        bytes: SLACK,
        mapped: false,
    });
    Residency { terms }
}

/// The whole tail's residency for this build, with the two Parquet figures read from footers.
///
/// **Footers only.** A row count and a column's uncompressed size both live in the file's metadata,
/// so this opens every input and reads none of them. A file that cannot be opened, or a column that
/// is not in it, contributes zero rather than refusing: the pre-flight is an estimate, and a build
/// blocked because a footer would not parse is a worse outcome than one that under-reads.
pub(crate) fn model(args: &crate::BuildArgs, n: u64) -> Residency {
    let mut payloads = vec![0u64; args.schema.attributes.len()];
    for source in &args.attribute_sources {
        let Some(metadata) = footer(&source.path) else {
            continue;
        };
        for &index in &source.attributes {
            let Some(attribute) = args.schema.attributes.get(index) else {
                continue;
            };
            if !is_variable_width(attribute.ty) {
                continue;
            }
            payloads[index] = payloads[index]
                .saturating_add(uncompressed_column_bytes(&metadata, attribute.column()));
        }
    }
    let columns: Vec<ColumnCost> = args
        .schema
        .attributes
        .iter()
        .zip(payloads)
        .map(|(attribute, payload_bytes)| ColumnCost {
            ty: attribute.ty,
            payload_bytes,
            // The same test the emit itself makes, called rather than restated: a text column
            // earns an index exactly where it is owed postings.
            text_index: attribute.ty == ScalarType::Text
                && crate::pipeline::postings_are_owed(&args.schema, attribute),
        })
        .collect();
    let member_rows = args
        .layer_inputs
        .iter()
        .filter_map(|layer| layer.members.as_ref())
        .filter_map(|members| footer(&members.path))
        .map(|metadata| metadata.file_metadata().num_rows().max(0) as u64)
        .sum();
    entity_order_residency(n, &columns, member_rows)
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

    fn column(ty: ScalarType, payload: u64) -> ColumnCost {
        ColumnCost {
            ty,
            payload_bytes: payload,
            text_index: false,
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
            column(ScalarType::Text, text_bytes_per_item * n),
            column(ScalarType::U32, 0),
        ];
        entity_order_residency(n, &columns, (2 * n) + (34 * n / 10) + n)
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

    /// **A declared column is reported and not charged.** The whole of a text column — its
    /// per-entity offsets, its presence bits and every character of the corpus's prose — is a file
    /// under `.build-tmp/`, so it appears in the breakdown at its full size and adds nothing to the
    /// figure `--memory-budget` is compared against.
    #[test]
    fn a_declared_column_is_reported_as_mapped_and_charged_at_nothing() {
        let n = 10_000_000;
        let bare = entity_order_residency(n, &[], 0);
        let with_text = entity_order_residency(n, &[column(ScalarType::Text, 400 * n)], 0);
        assert_eq!(with_text.total(), bare.total());
        let term = with_text
            .terms
            .iter()
            .find(|t| t.mapped)
            .expect("the column is a term of its own");
        assert_eq!(term.bytes, 8 * n + n.div_ceil(8) + 400 * n);
        assert!(
            with_text.describe().contains("(mapped)"),
            "the breakdown must say which terms are files: {}",
            with_text.describe()
        );
    }

    /// **A text index costs a second set of files, at the same time as the first.** The runs spill
    /// while the column they are tokenised from is still resident, so the disk pre-flight's column
    /// phase has to see both — and neither may reach the memory figure.
    #[test]
    fn a_text_index_carries_its_runs_beside_the_column() {
        let n = 10_000_000;
        let plain = ColumnCost {
            ty: ScalarType::Text,
            payload_bytes: 400 * n,
            text_index: false,
        };
        let indexed = ColumnCost {
            text_index: true,
            ..plain
        };
        let column_bytes = 8 * n + n.div_ceil(8) + 400 * n;

        let without = entity_order_residency(n, &[plain], 0);
        assert_eq!(without.mapped(), column_bytes);

        let with = entity_order_residency(n, &[indexed], 0);
        assert_eq!(with.mapped(), 2 * column_bytes);
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

    /// **The published memberships are charged and the spill is reported.** A member row is
    /// Roaring in the store and a delta in two files under `.build-tmp/`, and only the first is
    /// memory the machine must have — a model that kept charging the second would refuse builds
    /// that now fit, which is the failure mode of carrying a cost model past the thing it
    /// modelled.
    #[test]
    fn a_member_row_is_charged_where_it_is_resident_and_reported_where_it_is_a_file() {
        let n = 10_000_000;
        let rows = 64_000_000;
        let without = entity_order_residency(n, &[], 0);
        let with = entity_order_residency(n, &[], rows);
        assert_eq!(
            with.total() - without.total(),
            rows * BYTES_PER_MEMBER_ROW,
            "only the Roaring copies are memory"
        );
        assert_eq!(
            with.mapped() - without.mapped(),
            rows * SPILLED_BYTES_PER_MEMBER_ROW,
            "the runs and the merged table are disk"
        );
        assert!(
            with.describe().contains("member spill"),
            "the spill must be a named term of its own: {}",
            with.describe()
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
            first.contains("member row"),
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
        let model = model(&args, N);
        println!("model: {} MiB{}", model.total() >> 20, model.describe());
        crate::build_observed(&args, &Trace).unwrap();
        println!(
            "observed build peak: {} MiB",
            crate::observer::peak_rss_kib() / 1024
        );
    }
}
