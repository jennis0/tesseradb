//! Build-stage observation.
//!
//! `tessera build` is twelve numbered stages (see `pipeline.rs`), and "blank-database ingest as a
//! function of point count" is really a question about *which* of them bends with scale: the
//! sorts are n log n, the digest pass is linear in bytes, and several passes are driven by the
//! size of the source files rather than by `--limit` at all.
//!
//! **An observer rather than a feature.** [`build`](crate::build) delegates to
//! [`build_observed`] with a no-op, so there is no API break, no `#[cfg]` in the pipeline, and no
//! second code path that could drift from the one that ships. The observer sees only durations
//! and counts — nothing derived from the corpus content.

use std::time::Duration;

/// The twelve pipeline stages, in execution order.
///
/// Names match the `// ---- n. ...` comments in `pipeline.rs`. Adding a stage means adding a
/// variant; renaming one means changing both, which is the point — a stage that exists in the
/// code and not here would silently vanish from every measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStage {
    /// 1. Two passes over the points file, plus a sort and a duplicate check.
    SourceIds,
    /// 2. One pass over the pairs file, interning descriptors.
    Dictionary,
    /// 3. One pass over the pairs file, packing `ordinal << 32 | term_id`, then sort and dedup.
    PairsPack,
    /// 3b. **The one pass over the points file's geometry**, scattered by ordinal — early, because
    ///    the signature sort breaks ties on the Morton code (decision 0073) and entity ids do not
    ///    exist yet. It is put into entity order by [`BuildStage::Assignment`], which is walking
    ///    both indices anyway; there is no second geometry pass and no permute stage.
    GeometryRead,
    /// 4. The signature sort — permanent under I9, and the reason entity IDs cannot be
    ///    reassigned later.
    SignatureSort,
    /// 5. Entity id = position in the signature order.
    Assignment,
    /// 6. One pass over the pairs file, writing `postings.arrow` and `pairs.parquet`.
    PostingsWrite,
    /// 7. The external-id sidecar and its locator.
    ExternalIds,
    /// 8. Geometry permuted from ordinal into entity order, plus the declared attribute tail. No
    ///    points-file I/O: [`BuildStage::GeometryRead`] did the reading.
    AttributeTail,
    /// 8b. Entity-space filter postings, one file per `index = true` column. Its own
    ///    stage rather than a rider on `PostingsWrite`, which runs before the attribute values
    ///    have been read; zero-length for a schema that declares no filterable column.
    FilterPostings,
    /// 8c. The declared layers and their artifacts: the member tables read, resolved against the
    ///    entity ids this build assigned, and published through the same registry the control
    ///    plane runs.
    ///
    ///    **Its own stage because it owned an unattributed share of the peak.** It ran inside
    ///    [`BuildStage::AttributeTail`], which is where the campaign's OOM kills landed
    ///    (`probes/2026-08-22-artifact-serving-e2e/` finding 4) — and a stage boundary is what lets
    ///    an observer say which of the two it was.
    Layers,
    /// 9. The tiler sort: `(morton, tessera_id)` ascending.
    TilerSort,
    /// 10. Segment files: `morton.u32`, `permutation.bin`, `columns.arrow`, and their fsyncs.
    SegmentWrite,
    /// 11. Manifests — including a full SHA-256 re-read of every byte written.
    Manifests,
}

impl BuildStage {
    pub fn name(&self) -> &'static str {
        match self {
            BuildStage::SourceIds => "source_ids",
            BuildStage::Dictionary => "dictionary",
            BuildStage::PairsPack => "pairs_pack",
            BuildStage::GeometryRead => "geometry_read",
            BuildStage::SignatureSort => "signature_sort",
            BuildStage::Assignment => "assignment",
            BuildStage::PostingsWrite => "postings_write",
            BuildStage::ExternalIds => "external_ids",
            BuildStage::AttributeTail => "attribute_tail",
            BuildStage::FilterPostings => "filter_postings",
            BuildStage::Layers => "layers",
            BuildStage::TilerSort => "tiler_sort",
            BuildStage::SegmentWrite => "segment_write",
            BuildStage::Manifests => "manifests",
        }
    }

    pub const ALL: [BuildStage; 14] = [
        BuildStage::SourceIds,
        BuildStage::Dictionary,
        BuildStage::PairsPack,
        BuildStage::GeometryRead,
        BuildStage::SignatureSort,
        BuildStage::Assignment,
        BuildStage::PostingsWrite,
        BuildStage::ExternalIds,
        BuildStage::AttributeTail,
        BuildStage::Layers,
        BuildStage::FilterPostings,
        BuildStage::TilerSort,
        BuildStage::SegmentWrite,
        BuildStage::Manifests,
    ];
}

/// Notified as each stage completes.
///
/// `rows` is whatever the stage counted — items, pairs, terms — and is stage-specific rather than
/// uniform, because a uniform "rows" would be a lie for half of them. `peak_rss_kib` is sampled
/// at stage end, which is enough to see which stage owns the build's high-water mark.
pub trait BuildObserver: Send + Sync {
    fn stage_end(&self, stage: BuildStage, elapsed: Duration, rows: u64, peak_rss_kib: u64);
}

/// The default: observes nothing, costs nothing.
pub struct NoopObserver;

impl BuildObserver for NoopObserver {
    #[inline(always)]
    fn stage_end(&self, _: BuildStage, _: Duration, _: u64, _: u64) {}
}

/// Peak RSS in KiB from `/proc/self/status`'s `VmHWM`, or 0 where unavailable.
pub(crate) fn peak_rss_kib() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Times a stage and reports it. Held by the pipeline across each stage's body.
pub(crate) struct StageTimer<'a> {
    observer: &'a dyn BuildObserver,
    start: std::time::Instant,
}

impl<'a> StageTimer<'a> {
    pub(crate) fn new(observer: &'a dyn BuildObserver) -> Self {
        StageTimer {
            observer,
            start: std::time::Instant::now(),
        }
    }

    /// Close the current stage and open the next. `rows` is this stage's count.
    pub(crate) fn end(&mut self, stage: BuildStage, rows: u64) {
        self.observer
            .stage_end(stage, self.start.elapsed(), rows, peak_rss_kib());
        self.start = std::time::Instant::now();
    }
}
