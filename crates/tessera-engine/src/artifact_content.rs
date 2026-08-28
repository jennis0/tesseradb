//! The supplied content of a published level, read once per level instead of once per served
//! artifact.
//!
//! An artifact's supplied content — its name, its label — lives in the record blob at the
//! artifact's own entity, and the blob's read unit is a zstd block: one `fields_of` decompresses
//! ~256 KiB to return one row. That is the right shape for a drill-down, which asks about one
//! artifact, and the wrong one for a viewport, which serves thousands. **Measured on the GeoNames
//! bundle** (`admin/hierarchy`, zoom 7 over the eastern United States, nothing cold): 2 518 served
//! artifacts at level 2 cost 408 ms against 1.3 ms for the same viewport with no layer, and 23 821
//! at level 3 cost 3.75 s — ≈163 µs per artifact, all of it this read, and unchanged under
//! `artifact_rows: "identity"`, which materialises nothing but still had to prove the content was
//! there.
//!
//! So a level's contents are gathered in one pass over the blob — block by block, in entity order,
//! each block decompressed once ([`tessera_filter::RecordStack::for_each_row_in`]) — and held.
//! The same two requests are then **6.7 ms and 33 ms**, and the pass that buys that is 53 ms for
//! the whole of that level, paid by the first request that serves any artifact in it.
//!
//! **What is held, and its bound.** One entry per `(layer, level)`: the utf8 fields of every
//! entity the level has reserved that carries a row, tagged as the blob tags them. That is the
//! level's artifact count × its declared content, plus ~48 bytes of per-artifact overhead — the
//! whole of GeoNames' five levels is 464 655 names, ≈20 MB. Unbounded in the sense the lineages
//! beside it are unbounded: bounded by what the deployment published, not by what it is asked.
//!
//! **The alternative was an LRU of decompressed blocks**, and it is worse here for a reason that
//! is about the corpus rather than about caching: it pays off only when served artifacts share
//! blocks, which holds for a dense pan and fails for exactly the request that hurts most — a
//! whole-level fetch, which touches every block once and evicts as it goes. The per-level table
//! touches each block once too, and then answers every later request from memory.
//!
//! **This is a read amortisation and nothing else.** The table holds artefact bytes, not a
//! principal's view of them: which content a viewer is served is decided by
//! `crate::artifacts::ArtifactView::verdict` before anything here is asked, and the same "every
//! declared kind or none" rule applies to a table hit and to a direct read alike. Nothing about a
//! principal keys it, and no artifact's content reaches a response because it is in here.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tessera_types::layer::ReservedRuns;

/// One `(layer, level)`'s supplied content: the tagged utf8 fields of every artifact in it that
/// carries a record row.
///
/// **Utf8 fields only, and a non-utf8 field is dropped rather than recorded as present.** The two
/// are one answer at the reader: a tag the row does not carry and a tag whose value is not text
/// both withhold the artifact, so a table that cannot tell them apart serves what a direct read
/// serves.
#[derive(Debug, Default)]
pub(crate) struct LevelContent {
    /// Ascending entity ids, one per artifact with a row — binary-searched at lookup.
    entities: Vec<u32>,
    /// Parallel to `entities`: that artifact's `(tag, text)` pairs, ascending by tag.
    rows: Vec<Vec<(u16, String)>>,
    /// The text this holds, for the gauge. Counts the strings, not the vectors around them.
    bytes: u64,
}

impl LevelContent {
    /// Read one level's contents out of the record blob.
    ///
    /// `runs` is the level's own reserved entity space, which is what makes this a *bounded* read
    /// rather than a walk of the blob: artifact ids are allocated downward from the ceiling in
    /// whole blocks, so a level is one or a few contiguous ranges and the blob is in entity order.
    ///
    /// A malformed blob refuses at the read and leaves the level with no contents here — every
    /// artifact in it is then withheld, which is the same fail-closed answer the direct read gives
    /// for the same bytes.
    pub(crate) fn build(records: &tessera_filter::RecordStack, runs: &ReservedRuns) -> Self {
        let mut wanted = croaring::Bitmap::new();
        for run in runs.runs() {
            let (Ok(start), Ok(end)) = (u32::try_from(run.start), u32::try_from(run.end)) else {
                continue;
            };
            wanted.add_range(start..end);
        }
        let mut entities = Vec::new();
        let mut rows: Vec<Vec<(u16, String)>> = Vec::new();
        let mut bytes = 0u64;
        let read = records.for_each_row_in(&wanted, &mut |entity, fields| {
            let mut tagged: Vec<(u16, String)> = fields
                .into_iter()
                .filter_map(|field| match field.value {
                    tessera_filter::RecordValue::Utf8(text) => Some((field.tag, text)),
                    _ => None,
                })
                .collect();
            if tagged.is_empty() {
                return Ok(());
            }
            tagged.sort_unstable_by_key(|(tag, _)| *tag);
            bytes += tagged.iter().map(|(_, text)| text.len() as u64).sum::<u64>();
            entities.push(entity);
            rows.push(tagged);
            Ok(())
        });
        if let Err(e) = read {
            tracing::error!(
                error = %e,
                "ALARM: a level's supplied content could not be read from the record blob; every \
                 artifact in it is withheld until the blob is repaired"
            );
            return Self::default();
        }
        Self {
            entities,
            rows,
            bytes,
        }
    }

    /// This artifact's tagged fields, or `None` where it has no row — which withholds it, as an
    /// absent row does on the direct read.
    ///
    /// The entities arrive ascending from the blob, so this is a binary search.
    pub(crate) fn tagged(&self, entity: u32) -> Option<&[(u16, String)]> {
        let at = self.entities.binary_search(&entity).ok()?;
        Some(&self.rows[at])
    }

    /// How many artifacts this holds content for.
    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }

    /// The text bytes held — the gauge the bound above is stated in.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// One [`LevelContent`] per `(layer, level)`, rebuilt when that level or the bundle beneath it
/// moves.
///
/// **Two versions in the key, because two things can move the bytes.** The level's own version
/// covers a publication and the fold's retire — what artifacts the level holds and at which
/// ordinals. The generation's `segments_version` covers the blob those artifacts' rows live in: a
/// flush publishes the record extent that carries newly published content, and a fold rewrites the
/// blob whole. A coalesce moves neither, which is correct — it repacks the same `(entity, value)`
/// pairs into fewer files and no answer here changes.
///
/// Keyed per *deployment* like [`crate::cut::Lineages`] beside it, and for the same reason: what
/// an artifact's supplied content says is a property of what was published, never of who is
/// asking. Which of an artifact's ranked contents a principal is served is decided before this is
/// consulted.
#[derive(Debug, Default)]
pub(crate) struct LevelContents {
    cached: Mutex<BTreeMap<(String, u32), Held>>,
    builds: AtomicU64,
}

/// A held table and the two versions it was read at.
type Held = (u64, u64, Arc<LevelContent>);

/// What [`LevelContents`] is holding — the operator gauge, counting structures and bytes and
/// naming no layer, no artifact and no principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentCacheStats {
    /// Tables built since the engine opened — the cadence, as
    /// [`crate::cut::Lineages::builds`] is for lineages.
    pub builds: u64,
    /// Tables held right now; at most one per `(layer, level)`.
    pub held: usize,
    /// Artifacts those tables hold content for.
    pub artifacts: u64,
    /// The content text they hold, in bytes.
    pub bytes: u64,
}

impl LevelContents {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// This level's contents at these versions, reading them if what is held is stale.
    ///
    /// **The build runs outside this cache's lock**, as [`crate::cut::Lineages::get_or_build`]'s
    /// does and for the same reason: two threads racing one key both read, from the same versions,
    /// so the two results are equal and the waste is one pass over the blob rather than a wrong
    /// answer. It also runs outside the artifact store's lock, which the lineage's does not — this
    /// build decompresses megabytes, and holding the store through it would stall every write.
    pub(crate) fn get_or_build<F>(
        &self,
        layer: &str,
        level: u32,
        level_version: u64,
        segments_version: u64,
        build: F,
    ) -> Arc<LevelContent>
    where
        F: FnOnce() -> LevelContent,
    {
        let key = (layer.to_string(), level);
        if let Some((held_level, held_segments, content)) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if *held_level == level_version && *held_segments == segments_version {
                return Arc::clone(content);
            }
        }
        let content = Arc::new(build());
        self.builds.fetch_add(1, Ordering::Relaxed);
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (level_version, segments_version, Arc::clone(&content)));
        content
    }

    /// Drop every table held for one layer, when the layer is dropped — see
    /// [`crate::artifacts::ArtifactProjections::forget`], which carries the argument for all three
    /// of these caches. Retention only: a tombstoned name never resolves through the registry
    /// again, so nothing held here was reachable to be served.
    pub(crate) fn forget(&self, layer: &str) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, _), _| held != layer);
    }

    /// The gauges — see [`ContentCacheStats`].
    pub(crate) fn stats(&self) -> ContentCacheStats {
        let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        ContentCacheStats {
            builds: self.builds.load(Ordering::Relaxed),
            held: cached.len(),
            artifacts: cached.values().map(|(_, _, c)| c.len() as u64).sum(),
            bytes: cached.values().map(|(_, _, c)| c.bytes()).sum(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(text: &str) -> LevelContent {
        LevelContent {
            entities: vec![7],
            rows: vec![vec![(0, text.to_string())]],
            bytes: text.len() as u64,
        }
    }

    /// The key's whole job: an unchanged pair of versions answers from memory, and either one
    /// moving rebuilds. A table that outlived a publication would serve the level's *old* names,
    /// which is the failure this key exists to prevent and which no functional test would show —
    /// the names are all still names.
    #[test]
    fn either_version_moving_rebuilds_and_neither_moving_does_not() {
        let cache = LevelContents::new();

        let first = cache.get_or_build("a", 0, 1, 1, || content("published"));
        assert_eq!(first.tagged(7).unwrap()[0].1, "published");
        assert_eq!(cache.stats().builds, 1);

        // Same versions: held, and the closure is not run — asserted by the count rather than by
        // the value, since a rebuild here would produce the same value.
        let warm = cache.get_or_build("a", 0, 1, 1, || content("never read"));
        assert_eq!(warm.tagged(7).unwrap()[0].1, "published");
        assert_eq!(cache.stats().builds, 1);

        // A publication moves the level's version.
        let republished = cache.get_or_build("a", 0, 2, 1, || content("republished"));
        assert_eq!(republished.tagged(7).unwrap()[0].1, "republished");
        assert_eq!(cache.stats().builds, 2);

        // A flush or a fold moves the blob under it.
        let reflushed = cache.get_or_build("a", 0, 2, 2, || content("reflushed"));
        assert_eq!(reflushed.tagged(7).unwrap()[0].1, "reflushed");
        assert_eq!(cache.stats().builds, 3);

        assert_eq!(cache.stats().held, 1, "one entry per (layer, level)");
        cache.forget("a");
        assert_eq!(cache.stats().held, 0);
    }

    /// An entity the level reserved but that carries no row is an absence, not a neighbour's row.
    #[test]
    fn an_entity_with_no_row_is_absent_rather_than_its_neighbours() {
        let table = LevelContent {
            entities: vec![4, 9],
            rows: vec![
                vec![(0, "four".to_string())],
                vec![(0, "nine".to_string())],
            ],
            bytes: 8,
        };
        assert_eq!(table.tagged(4).unwrap()[0].1, "four");
        assert_eq!(table.tagged(9).unwrap()[0].1, "nine");
        assert!(table.tagged(5).is_none());
        assert!(table.tagged(10).is_none());
    }
}
