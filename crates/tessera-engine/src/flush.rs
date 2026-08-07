//! What a flush takes from the buffer, and the two states in which it takes nothing (§3.5).
//!
//! Planning is the executor's half of a flush: it runs against the live generation, decides which
//! buffered items acquire geometry, and hands an immutable plan to the background pool. Writing the
//! segment and publishing it are elsewhere — this module is the part where a disposition decides an
//! item's fate, which is the part that is invariant-bearing.
//!
//! ## The rules are relative to the buffer snapshot the flush took
//!
//! A delete accepted *after* the snapshot produces a deleted entity that **does** have a row,
//! hidden by its standing overlay entry alone. That is safe today only because nothing retires —
//! deletion denies never retire, there being no stamp ledger — and it is an obligation the
//! compaction spec inherits rather than a caveat this one absorbs.
//!
//! ## The two dispositions do different things, and uniformity here is fail-open
//!
//! Lifecycle §3.1 gives each a different relationship to the postings, so each gets a different
//! answer:
//!
//! - **Suppressed → flushed normally.** A suppression never touches postings and retires only on
//!   unsuppress, so a flush that skipped it would leave a later unsuppress with **nothing to
//!   reveal** — the item would have no row, and unsuppressing it would show nothing.
//! - **Deleted → never written into the segment.** The ID stays burned (I9), no row is created,
//!   and the deny entry stands.
//!
//! A third disposition, an evaluate entry, is **deleted** (decision 0048): a flush wrote the
//! buffered row's terms and let the entry stand, because writing the *entry's* current terms would
//! have been the fold — invariant-bearing, and compaction's. Nothing here needs that rule any more,
//! but the rule it protected still holds for the two above: this pass never folds.
//!
//! ## Every buffered row has a cell (§6)
//!
//! This module quantises whatever the buffer holds and does not re-check the extent. That is not an
//! omission: `Engine::accept_ingest` refuses an out-of-extent coordinate before anything is acked
//! or WAL-durable, at the boundary where rows enter the buffer, so the state a check here would
//! detect cannot arise. A second copy of the predicate is how the two would come to disagree —
//! `Quantisation::contains` is the one definition, and issue #72 (quantisation moves to
//! `SliceDescriptor`) is the change that would otherwise have to update both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tessera_authz::{write_delta_tier, DeltaTier, Dict, DictStreamWriter};
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::{BufferedItem, Overlay};
use tessera_spatial::tiler::{ScalarType, ScalarValue};
use tessera_store::manifest::{DictExtent, FileDigest, Quantisation};
use tessera_store::permutation::SegmentExtent;
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
use tessera_store::{write_flush_segment, FlushInput, FlushRow};
use tessera_types::{EntityId, IdentityKey, TermId};

use crate::Generation;

/// The tag rule a tier's postings use — `postings.arrow`'s, unchanged (see
/// [`tessera_authz::write_delta_tier`]). Taken from the bundle's own `small_term_threshold` would
/// be better still; it is a constant here because a flush's postings are small by construction
/// (one tick's arrivals) and the threshold only decides an encoding, never a content.
const SMALL_TERM_THRESHOLD: u32 = 32;

/// One flush's immutable plan: the items of one slice that will acquire geometry.
///
/// The slice is not carried: `plan_flush` is called per slice and the caller already holds it, so
/// a copy here would be a second answer to a question that has one.
#[derive(Debug)]
pub(crate) struct FlushPlan {
    /// **Ascending by entity id, deleted entities already removed.** Contiguity is I9's doing —
    /// ids are issued monotonically from the high-water — and it is what makes the segment's
    /// extent dense.
    ///
    /// The segment's entity range is this list's ends, and is deliberately not carried separately:
    /// a deleted entity at either end contributes no row, so a range taken from the *buffer's*
    /// bounds would claim one it does not have.
    pub(crate) items: Vec<(EntityId, BufferedItem)>,
}

/// Why a tick published nothing. Each is a distinct operator-facing condition, and two of them are
/// fail-closed postures rather than absences of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFlush {
    /// Nothing buffered for this slice, or everything buffered for it is deleted.
    NothingToFlush,
    /// **The WAL is poisoned** (§3.5). Under the apply-anyway rule an under-durable delete is in
    /// force in memory and was answered 500, and contracts §3.1's residual is that a restart makes
    /// the item visible again. A flush honouring such a delete would skip the entity and advance
    /// the watermark past it; replay would then discard the delete record, leaving the item in no
    /// segment and no buffer — the un-acked delete made **permanent**.
    ///
    /// Costs ingest visibility during WAL degradation, when nothing new is being made durable
    /// anyway.
    WalPoisoned,
    /// **The in-memory overlay has diverged from the durable WAL** (§7.2).
    ///
    /// `Wal::discard_undurable` deliberately does not un-apply — "a restart will not carry them" —
    /// so after an in-process recovery the node returns to `Running` while holding dispositions no
    /// record backs, and the poisoned gate no longer covers it. Publishing a manifest from that
    /// overlay would make a 500'd, never-acked deny **permanent**, contradicting contracts §3.1's
    /// residual.
    ///
    /// A diverged node keeps serving and keeps applying denies, but publishes no flush and rotates
    /// no WAL until it is restarted, alarmed throughout. Re-appending the divergent entries to
    /// converge the WAL was the alternative, and it is rejected because it produces a state **no
    /// restart could have produced** — which is lifecycle §4's central argument.
    OverlayDiverged,
    /// **A partition is serving a stepped-down side-manifest** (owner-ruled gate, 2026-08-04;
    /// write-path §5.6). A flush from this state assembles its manifest from the *older served*
    /// partition state and commits it at a higher `n`, permanently shadowing the stepped-past
    /// segment; and §7.1's buffer reconstruction against the served row space means the shadowed
    /// rows' WAL members are the only recovery material left. Publishing nothing while stepped
    /// down costs ingest visibility on a node whose newest manifest's files are damaged — the
    /// same trade the poisoned and diverged gates make, for the same reason.
    SteppedDown,
}

/// Plan a flush of `slice` against `generation`.
///
/// Pure: it reads the generation and nothing else, so the same generation always yields the same
/// plan. The two postures are passed in rather than read here, because they are the executor's
/// health and not the generation's.
pub(crate) fn plan_flush(
    generation: &Generation,
    slice: &str,
    wal_poisoned: bool,
    overlay_diverged: bool,
) -> Result<FlushPlan, NoFlush> {
    // The gates first, and before any work: a poisoned, diverged or stepped-down node publishes
    // nothing, and deciding that after building a plan would only mean building one to throw
    // away. Step-down is read off the generation's own bundle — it is bundle state, not executor
    // health — so it is checked here rather than passed in.
    if wal_poisoned {
        return Err(NoFlush::WalPoisoned);
    }
    if overlay_diverged {
        return Err(NoFlush::OverlayDiverged);
    }
    if generation
        .bundle
        .partitions
        .values()
        .any(|p| p.stepped_down())
    {
        return Err(NoFlush::SteppedDown);
    }

    let mut items: Vec<(EntityId, BufferedItem)> = generation
        .buffer
        .iter()
        .filter(|(entity, item)| item.slice == slice && !is_deleted(&generation.overlay, **entity))
        .map(|(entity, item)| (*entity, item.clone()))
        .collect();
    if items.is_empty() {
        return Err(NoFlush::NothingToFlush);
    }
    // The buffer is a hash map, so order is arbitrary until sorted. Ascending by entity id is what
    // `write_flush_segment` requires and what makes the extent dense.
    items.sort_unstable_by_key(|(entity, _)| entity.raw());

    Ok(FlushPlan { items })
}

/// Everything the background pool needs to turn a [`FlushPlan`] into durable files.
///
/// Taken from the generation on the executor thread and then **immutable**: the pool holds no
/// reference to live state, which is what makes "execute on the pool over immutable inputs" (§1.1)
/// true rather than a description of intent.
pub(crate) struct FlushContext {
    pub(crate) prefix_dir: PathBuf,
    pub(crate) partition: String,
    pub(crate) slice: String,
    pub(crate) seg_id: String,
    pub(crate) row_base: u32,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    pub(crate) quantisation: Quantisation,
    pub(crate) scalar_schema: Vec<(String, ScalarType)>,
    /// The dictionary the plan's terms were resolved against, and the one promotion extends.
    pub(crate) dict: Arc<Dict>,
    /// The descriptor bytes behind every **extension** term id this plan's items carry (§3.2).
    ///
    /// **The inverse of `DescriptorResolver`'s extension map, and only the part this plan needs.**
    /// The buffer holds resolved `TermId`s and not descriptors, which is what made promotion look
    /// like it needed a WAL read; the resolver has held the bytes all along, deduped and carried
    /// across replay so the assignment stays continuous for the process's lifetime. Restricted to
    /// the plan's own ids because a descriptor no flushed item references has no posting to write,
    /// and interning it would put a descriptor into a durable artefact for no reason.
    ///
    /// Empty in the steady state, and `dispatch_flushes` does not take the resolver's lock to
    /// build it unless some planned item actually carries an extension id.
    pub(crate) novel_descriptors: FxHashMap<TermId, Vec<u8>>,
    /// The plugin's declared `max_distinct_terms`. Promotion is the one place a *caller* can grow
    /// the dictionary, so it is the one bound that is enforced rather than declared — see
    /// [`promote`].
    pub(crate) max_distinct_terms: u64,
    pub(crate) prefix: String,
}

/// A flush whose files are durable, awaiting manifest assembly and the swap on the executor.
///
/// **The side-manifest is still the commit point (§7.3), and it is written at publication, not
/// here.** A crash while one of these is in flight leaves orphan files nothing references, and
/// the next tick re-plans; only once `publish_flush` has written the manifest does a crash leave
/// a bundle that opens at the new `n` with everything it names present. The manifest moved to
/// the executor because `n` cannot be allocated at plan time (a deny publication may take one
/// mid-flight) and the deny fields must reflect the overlay at publication — see the field docs
/// below.
pub(crate) struct CompletedFlush {
    pub(crate) partition: String,
    pub(crate) slice: String,
    /// The entity ids removed from the buffer at publication. **Exactly what was consumed**, not a
    /// range: the rebase removes these from the *then-current* buffer, whatever arrived while the
    /// flush ran (§1.2).
    pub(crate) consumed: Vec<EntityId>,
    pub(crate) segment: SegmentData,
    pub(crate) extent: SegmentExtent,
    /// **The manifest's ingredients, not a manifest.** Contracts §2.3 makes a side-manifest
    /// complete current state for its partition, and *current* is decided at publication: the
    /// executor assembles this into the live partition manifest, with deny fields serialised
    /// fresh from the overlay of the generation being published. A manifest cloned at plan time
    /// would carry the deny state of a snapshot the flush's own flight has outlived.
    pub(crate) descriptor: tessera_store::manifest::SegmentDescriptor,
    pub(crate) watermark: u64,
    pub(crate) entity_id_high_water: u64,
    pub(crate) external_id_run: String,
    pub(crate) locator_extent: tessera_store::manifest::LocatorExtent,
    /// `Some` iff this flush promoted (§3.2); its digest is already in `files`.
    pub(crate) dict_extent: Option<DictExtent>,
    /// Every file this flush wrote, prefix-relative, with its digest — computed on the pool.
    pub(crate) files: std::collections::BTreeMap<String, FileDigest>,
    pub(crate) tier: Arc<DeltaTier>,
    /// The tier's prefix-relative path — `deltas`' entry for it (contracts §2.3 r18). Carried
    /// rather than re-derived at publication, because after a coalesce a tier's path is no longer
    /// a function of any segment's `seg_id`.
    pub(crate) tier_path: String,
    /// The tier measured as encoded (`FragmentationTally::of_tier`) — contracts §3.4's
    /// `fragmentation`, at the scope where between-window scatter is visible. Computed on the
    /// pool beside the write it measures; recorded by the executor only if the flush publishes.
    pub(crate) tier_tally: tessera_lifecycle::window::FragmentationTally,
    /// The dictionary including this flush's promotions (§3.2), republished with the geometry.
    pub(crate) dict: Arc<Dict>,
    /// `Some(len)` if this flush wrote a dictionary extent, carrying the dictionary length its
    /// ordinals were assigned from; `None` if it promoted nothing.
    ///
    /// **What the publication guard reads.** A dict extent's ordinals are positions in the
    /// concatenation of `dict_extents` in listed order, so they are correct only if the extent
    /// lands where the flush assumed. A flush that promoted nothing has no such dependency — its
    /// tier names only ordinals below `len`, which append-only extension preserves — so the guard
    /// is scoped to this being `Some`.
    pub(crate) promoted_from_dict_len: Option<u32>,
    pub(crate) prefix: String,
}

/// Turn a plan into durable files. **Runs on the background pool, over immutable inputs** (§1.1).
///
/// The order is §7.3's, and the side-manifest is last because it is the commit point: a crash
/// before it leaves orphan files nothing references, and replay re-flushes deterministically.
pub(crate) fn execute_flush(
    plan: FlushPlan,
    ctx: FlushContext,
) -> Result<CompletedFlush, FlushFailed> {
    let consumed: Vec<EntityId> = plan.items.iter().map(|(entity, _)| *entity).collect();

    // ---- promotion (§3.2) -------------------------------------------------------------------
    //
    // `buffer.rs` allocates term ids for descriptors the dictionary has never seen from the top of
    // the `u32` range downward, precisely so they are unsatisfiable: a novel descriptor can buffer
    // an item but can never make it visible. This is where that ends for the items being flushed.
    //
    // **The tier's postings are written in promoted ordinals, never extension ids.** An extension
    // id is process-local and its meaning changes at the next replay, so a tier carrying one would
    // name whatever descriptor interned into that slot next — the same hazard `buffer.rs` counts
    // downward from `u32::MAX` to avoid, arriving by a different route.
    let promotion = promote(&plan, &ctx)?;
    let promoted_from = promotion.extent.as_ref().map(|_| ctx.dict.len());

    // ---- the segment, its extents and its locator -------------------------------------------
    let rows: Vec<FlushRow> = plan
        .items
        .iter()
        .map(|(entity, item)| FlushRow {
            entity_id: *entity,
            external_id: item.external_id.clone(),
            x: item.x,
            y: item.y,
            scalars: item.scalars.iter().map(to_scalar_value).collect(),
        })
        .collect();
    let out = write_flush_segment(
        &ctx.prefix_dir,
        &ctx.partition,
        &ctx.slice,
        FlushInput {
            seg_id: &ctx.seg_id,
            rows,
            quantisation: ctx.quantisation,
            identity_key: &ctx.identity_key,
            shard_id: ctx.shard_id,
            scalar_schema: &ctx.scalar_schema,
            row_base: ctx.row_base,
        },
    )
    .map_err(|e| FlushFailed(format!("segment: {e}")))?;

    // ---- the delta postings tier ------------------------------------------------------------
    let tier_tally = tessera_lifecycle::window::FragmentationTally::of_tier(
        &promotion.postings,
        plan.items.len() as u64,
    );
    let tier_rel = format!(
        "partitions/{}/slices/{}/segments/{}/delta.arrow",
        ctx.partition, ctx.slice, ctx.seg_id
    );
    let tier_path = ctx.prefix_dir.join(&tier_rel);
    write_delta_tier(&tier_path, &promotion.postings, SMALL_TERM_THRESHOLD)
        .map_err(|e| FlushFailed(format!("delta tier: {e}")))?;
    let tier =
        Arc::new(DeltaTier::open(&tier_path).map_err(|e| FlushFailed(format!("tier: {e}")))?);

    // ---- the manifest's ingredients, not the manifest ---------------------------------------
    //
    // **This function no longer writes the side-manifest.** It computes everything the manifest
    // will name — the files and their digests, all on the pool where the IO belongs — and the
    // executor assembles and writes it at publication (`Executor::publish_flush`). Two reasons,
    // both structural. `n` cannot be allocated here: a deny publication may take one while this
    // flush is in flight, and a manifest committed at a lower `n` than the newest is a manifest a
    // restore never reads — the segment silently lost. And the deny fields must reflect the
    // overlay at *publication*, not at plan time, which is a thing only the executor holds.
    let mut files = out.files;
    files.insert(
        tier_rel.clone(),
        digest_of(&tier_path).map_err(FlushFailed)?,
    );
    let dict_extent = match promotion.extent {
        Some(extent) => {
            files.insert(
                extent.path.clone(),
                digest_of(&ctx.prefix_dir.join(&extent.path)).map_err(FlushFailed)?,
            );
            Some(extent)
        }
        None => None,
    };

    let seg_dir = segment_dir(&ctx);
    let segment = SegmentData {
        seg_id: ctx.seg_id.clone(),
        row_count: out.segment.row_count,
        morton: MortonSlice::load(&seg_dir.join("morton.u32"))
            .map_err(|e| FlushFailed(format!("morton: {e}")))?,
        columns: ColumnsRef::load(&seg_dir.join("columns.arrow"))
            .map_err(|e| FlushFailed(format!("columns: {e}")))?,
    };

    Ok(CompletedFlush {
        partition: ctx.partition,
        slice: ctx.slice,
        consumed,
        segment,
        extent: out.extent,
        descriptor: out.segment,
        watermark: out.watermark,
        entity_id_high_water: out.entity_id_high_water,
        external_id_run: out.external_id_run,
        locator_extent: out.locator_extent,
        dict_extent,
        files,
        tier,
        tier_path: tier_rel,
        tier_tally,
        dict: promotion.dict,
        promoted_from_dict_len: promoted_from,
        prefix: ctx.prefix,
    })
}

/// Why a flush produced nothing. **Every failure is "nothing happened, retry next tick"** (§10):
/// the side-manifest is the only commit point, so a failure before it leaves orphan files nothing
/// references and a failure after it cannot happen — there is nothing left to fail.
#[derive(Debug)]
pub(crate) struct FlushFailed(pub(crate) String);

impl std::fmt::Display for FlushFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What promotion produced: the dictionary to republish, the extent naming the new ordinals, and
/// the tier's postings in those ordinals.
struct Promotion {
    dict: Arc<Dict>,
    extent: Option<DictExtent>,
    /// `(term, entities)` ascending by term — [`write_delta_tier`]'s contract.
    postings: Vec<(TermId, Vec<u32>)>,
}

/// Promote every extension-id descriptor the plan carries to a durable dictionary ordinal, and
/// express the plan's postings in those ordinals (§3.2).
///
/// **Two fail-closed consequences, neither obvious and both left standing.** A promoted descriptor
/// is satisfiable only by sessions authorised *after* this flush, because `satisfied` is fixed per
/// session at authorise — which is also what makes §3.4's patch-equals-a-rebuild equality hold. And
/// an item still buffered under an old extension id for an already-promoted descriptor stays
/// invisible until *its own* flush, even to a viewer holding the term.
///
/// **The dictionary is consulted before anything is interned, and that is the writer half of the
/// no-duplicate rule.** An extent that repeated a descriptor the dictionary already holds would
/// make [`Dict::load`] and [`Dict::load_extending`] assign different ordinals — a running process
/// and the same bundle reopened disagreeing about what every ordinal after the repeat means. The
/// reader closes that too ([`Dict::load`]'s doc); this closes it at the source, and either alone
/// would leave a viewer being served another compartment's items with no error anywhere.
///
/// **An extension id with no descriptor fails the flush.** It cannot happen — the resolver's map
/// only grows, and rotation retains the WAL records of anything still buffered, so replay
/// re-interns every id a plan can name — but silently dropping a term is precisely the bug this
/// function existed to have, and it must not survive as the error path.
fn promote(plan: &FlushPlan, ctx: &FlushContext) -> Result<Promotion, FlushFailed> {
    let dict_len = ctx.dict.len();
    let mut by_term: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    // Descriptors this flush interns, in assignment order — the extent file's contents, and the
    // sequence `Dict::extended_with` is handed. Order is the two artefacts' shared contract.
    let mut interned: Vec<Vec<u8>> = Vec::new();
    let mut assigned: FxHashMap<u32, u32> = FxHashMap::default();

    for (entity, item) in &plan.items {
        let Ok(entity) = u32::try_from(entity.raw()) else {
            return Err(FlushFailed(format!(
                "entity {} does not fit the u32 posting space (I9's ceiling)",
                entity.raw()
            )));
        };
        for term in &item.terms {
            // Below the dictionary's length: already a durable ordinal, nothing to do.
            let ordinal = if term.raw() < dict_len {
                term.raw()
            } else if let Some(&ordinal) = assigned.get(&term.raw()) {
                ordinal
            } else {
                let descriptor = ctx.novel_descriptors.get(term).ok_or_else(|| {
                    FlushFailed(format!(
                        "extension term {} has no descriptor in this flush's snapshot — see \
                         promote()'s doc; a term must never be dropped silently",
                        term.raw()
                    ))
                })?;
                let ordinal = match ctx.dict.lookup(descriptor) {
                    // An earlier flush already promoted it. Use that ordinal and write nothing:
                    // this is both the no-duplicate rule and what makes an item buffered under a
                    // stale extension id become visible at its own flush (§3.2).
                    Some(existing) => existing.raw(),
                    None => {
                        let next = u64::from(dict_len) + interned.len() as u64;
                        // **The one plugin bound that is enforced, not declared.** Promotion is
                        // the only path by which a caller grows the dictionary, and
                        // `EXTENSION_ID_START > max_distinct_terms` is what keeps a dictionary
                        // ordinal from ever aliasing a live extension id. Past the declaration
                        // that assertion stops holding, so this fails the flush — buffer
                        // retained, ingest sheds at its own bound, which is the intended
                        // backpressure.
                        if next >= ctx.max_distinct_terms {
                            return Err(FlushFailed(format!(
                                "promoting this flush's novel descriptors would carry the \
                                 dictionary to {next}, at or past the plugin's declared \
                                 max_distinct_terms of {}; refusing rather than assigning an \
                                 ordinal that could alias an extension id",
                                ctx.max_distinct_terms
                            )));
                        }
                        interned.push(descriptor.clone());
                        next as u32
                    }
                };
                assigned.insert(term.raw(), ordinal);
                ordinal
            };
            by_term.entry(ordinal).or_default().push(entity);
        }
    }

    let mut postings = Vec::with_capacity(by_term.len());
    for (term, mut entities) in by_term {
        // `encode_posting` hard-fails on a non-strictly-ascending list, and a buffered item's
        // descriptors are not deduplicated on the write path, so this is required rather than
        // defensive. Set semantics, so it folds no authorisation state.
        entities.sort_unstable();
        entities.dedup();
        postings.push((TermId::new(term), entities));
    }

    if interned.is_empty() {
        // The steady state: no extent, no dictionary clone, nothing published but the tier.
        return Ok(Promotion {
            dict: Arc::clone(&ctx.dict),
            extent: None,
            postings,
        });
    }

    let seg_dir = segment_dir(ctx);
    std::fs::create_dir_all(&seg_dir).map_err(|e| FlushFailed(format!("dict extent dir: {e}")))?;
    let mut writer = DictStreamWriter::new(&seg_dir);
    for descriptor in &interned {
        writer.append(descriptor);
    }
    writer
        .finish()
        .map_err(|e| FlushFailed(format!("dict extent: {e}")))?;

    Ok(Promotion {
        // **Built from the same sequence that named the tier, never re-read from the file just
        // written.** Nothing compares the two, so a bad read would silently *become* the live
        // assignment while the tier holds the intended one. That the file agrees is a restart
        // property, proved by a test against the file.
        dict: Arc::new(ctx.dict.extended_with(&interned)),
        extent: Some(DictExtent {
            path: format!(
                "partitions/{}/slices/{}/segments/{}/terms-0.dict",
                ctx.partition, ctx.slice, ctx.seg_id
            ),
            records: interned.len() as u64,
        }),
        postings,
    })
}

/// This flush's segment directory. Both the segment writer and promotion address it; naming it
/// once keeps them from drifting apart.
fn segment_dir(ctx: &FlushContext) -> PathBuf {
    ctx.prefix_dir
        .join("partitions")
        .join(&ctx.partition)
        .join("slices")
        .join(&ctx.slice)
        .join("segments")
        .join(&ctx.seg_id)
}

fn to_scalar_value(scalar: &WalScalar) -> ScalarValue {
    match scalar {
        WalScalar::U64(v) => ScalarValue::U64(*v),
        WalScalar::F32(v) => ScalarValue::F32(*v),
        WalScalar::Utf8(v) => ScalarValue::Utf8(v.clone()),
    }
}

/// One file's size and hex SHA-256, by reading it back — `tessera_store::digest_of` with this
/// crate's error type.
///
/// **One definition, in the crate that owns the manifest format.** Three copies of this existed,
/// here, in `coalesce.rs` and in `tessera-store`, and all three read the whole file into memory;
/// the fold's pass 5 is where that stopped being affordable (probe P1), and a fix applied to one
/// copy would have left the other two. `pub(crate)` because compaction's pass 5 digests its own
/// outputs the same way — see `crate::compact::execute`, which states why the digest is taken from
/// a re-read rather than computed as the bytes are written.
pub(crate) fn digest_of(path: &Path) -> Result<FileDigest, String> {
    tessera_store::digest_of(path).map_err(|e| format!("digest {}: {e}", path.display()))
}

/// Whether `entity` is deleted as of this overlay.
///
/// **Only `deleted` excludes an item from a flush.** `suppressed` does not — the row must exist for
/// a later unsuppress to reveal. Reading any other field here is the fold arriving as a
/// simplification; see this module's doc.
///
/// Unreachable in the steady state and kept deliberately: a delete now drops its row from the
/// buffer as it applies (`tessera_lifecycle::overlay::drop_deleted`), so no live buffer holds a
/// deleted row for this to find. This is the statement of *which disposition* excludes an item,
/// which is the invariant-bearing half, and it must not follow the buffer's shape.
fn is_deleted(overlay: &Overlay, entity: EntityId) -> bool {
    overlay.is_deleted(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use std::collections::{BTreeMap, HashMap};

    use tessera_lifecycle::wal::{ChangeOp, WalRow, WalScalar};
    use tessera_lifecycle::IngestBuffer;
    use tessera_store::manifest::{IdentityDescriptor, Manifest, Quantisation};
    use tessera_store::Bundle;
    use tessera_types::{TermId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

    const SLICE: &str = "s0";

    fn item(terms: &[u32]) -> BufferedItem {
        BufferedItem {
            terms: terms.iter().map(|t| TermId::new(*t)).collect(),
            slice: SLICE.to_string(),
            x: 0.5,
            y: 0.5,
            scalars: vec![WalScalar::U64(1)],
            external_id: None,
            wal_pos: None,
        }
    }

    fn buffer_with(buffered: &[(u64, BufferedItem)]) -> IngestBuffer {
        let mut buffer = IngestBuffer::new();
        for (entity, item) in buffered {
            let row = WalRow {
                external_id: Some(format!("ext-{entity}").into_bytes()),
                entity_id: EntityId::new(*entity),
                slice: item.slice.clone(),
                descriptors: Vec::new(),
                x: item.x,
                y: item.y,
                scalars: item.scalars.clone(),
            };
            buffer.insert_row_with_terms(&row, item.terms.clone());
        }
        buffer
    }

    /// A generation over `buffered`, with `changes` applied to its overlay.
    ///
    /// The bundle is empty: `plan_flush` reads the buffer and the overlay and nothing else, so a
    /// real one would make these tests about the fixture instead.
    fn generation_with(
        buffered: &[(u64, BufferedItem)],
        changes: &[(u64, ChangeOp)],
    ) -> Generation {
        let mut overlay = Overlay::new();
        for (entity, op) in changes {
            overlay.apply(EntityId::new(*entity), *op);
        }
        generation_of(overlay, buffer_with(buffered))
    }

    fn generation_of(overlay: Overlay, buffer: IngestBuffer) -> Generation {
        let manifest = Manifest {
            bundle_format: 1,
            created_at: "2026-08-02T00:00:00Z".to_string(),
            data_plugin_hash: "builtin:passthrough:1".to_string(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            small_term_threshold: 32,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            entity_id_high_water: 0,
            identity: IdentityDescriptor {
                construction: IDENTITY_CONSTRUCTION.to_string(),
                rounds: IDENTITY_ROUNDS,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            slices: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: BTreeMap::new(),
        };
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let postings_path = dir.path().join("postings.arrow");
        tessera_authz::write_postings(&postings_path, &[], 32).expect("an empty postings file");
        let (fragments, external_index) = crate::synthetic_generation_parts();
        Generation {
            prefix: "v00000".to_string(),
            segments_version: 0,
            watermark: 0,
            bundle: Arc::new(Bundle {
                manifest,
                partitions: HashMap::new(),
            }),
            dict: Arc::new(tessera_authz::Dict::load(&[]).expect("an empty dict")),
            postings: Arc::new(
                tessera_authz::PostingsReader::open(&postings_path, false).expect("it opens"),
            ),
            fragments,
            external_index,
            delta_postings: Vec::new(),
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
            // The fixture bundle carries no slices, so a fresh derivation is empty.
            denied: Arc::new(crate::DenyMask::default()),
        }
    }

    fn plan(generation: &Generation) -> Result<FlushPlan, NoFlush> {
        plan_flush(generation, SLICE, false, false)
    }

    /// **A suppression never touches postings and retires only on unsuppress**, so a flush that
    /// skipped it would leave a later unsuppress with nothing to reveal: no row would exist, and
    /// unsuppressing the item would show nothing at all.
    #[test]
    fn a_suppressed_entity_is_flushed_so_a_later_unsuppress_has_something_to_reveal() {
        let generation = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Suppress)]);
        let plan = plan(&generation).expect("a suppressed item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].0, EntityId::new(7));
    }

    /// A deletion's ID stays burned (I9), no row is created, and the deny entry stands.
    #[test]
    fn a_deleted_entity_acquires_no_row() {
        let generation = generation_with(
            &[(7, item(&[1])), (8, item(&[1]))],
            &[(7, ChangeOp::Delete)],
        );
        let plan = plan(&generation).expect("the undeleted item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(
            plan.items[0].0,
            EntityId::new(8),
            "the deleted entity contributes no row"
        );
    }

    /// A deletion accepted *after* the snapshot is a different case, and is not this one's: it
    /// produces a deleted entity that **does** have a row, hidden by its overlay entry alone. Safe
    /// only because nothing retires, and an obligation the compaction spec inherits.
    #[test]
    fn a_delete_arriving_after_the_plan_does_not_unwrite_the_row() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        let plan = plan(&generation).expect("nothing is denied at the snapshot");
        assert_eq!(plan.items.len(), 1, "the row is planned");
        // A later delete cannot reach this plan: it is a value, taken from one generation.
        let later = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Delete)]);
        assert!(matches!(
            plan_flush(&later, SLICE, false, false),
            Err(NoFlush::NothingToFlush)
        ));
    }

    /// **A `WalPoisoned` node publishes nothing** (§3.5). A flush honouring an under-durable
    /// delete would skip the entity and advance the watermark past it; replay would then discard
    /// the delete record, leaving the item in no segment and no buffer — the un-acked delete made
    /// permanent, against contracts §3.1's residual that a restart makes it visible again.
    #[test]
    fn a_wal_poisoned_node_plans_nothing() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        assert!(matches!(
            plan_flush(&generation, SLICE, true, false),
            Err(NoFlush::WalPoisoned)
        ));
    }

    /// **A node whose overlay has diverged from its durable WAL publishes nothing** (§7.2), and
    /// the poisoned gate does not cover it: `discard_undurable` returns the node to `Running`
    /// while it still holds dispositions no record backs.
    #[test]
    fn a_diverged_node_plans_nothing_even_though_its_wal_is_healthy() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        assert!(matches!(
            plan_flush(&generation, SLICE, false, true),
            Err(NoFlush::OverlayDiverged)
        ));
    }

    /// Items of another slice are not this slice's to flush: a segment's entity range is
    /// contiguous only within one (§2.1).
    #[test]
    fn another_slices_items_are_left_alone() {
        let mut other = item(&[1]);
        other.slice = "elsewhere".to_string();
        let generation = generation_with(&[(7, other)], &[]);
        assert!(matches!(plan(&generation), Err(NoFlush::NothingToFlush)));
    }

    /// Ascending by entity id, because `write_flush_segment` requires it and because the extent is
    /// dense over the range. The buffer is a hash map, so nothing else establishes the order.
    #[test]
    fn the_plan_is_ascending_by_entity_id() {
        let generation = generation_with(&[(9, item(&[1])), (3, item(&[1])), (7, item(&[1]))], &[]);
        let plan = plan(&generation).unwrap();
        let ids: Vec<u64> = plan.items.iter().map(|(e, _)| e.raw()).collect();
        assert_eq!(ids, vec![3, 7, 9]);
    }
}
