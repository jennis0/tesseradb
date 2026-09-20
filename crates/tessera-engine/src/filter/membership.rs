use croaring::Bitmap;
use rustc_hash::FxHashMap;
use tessera_filter::ColumnPostings;
use tessera_types::AttrLocalId;

use super::columns::FilterColumns;
use super::error::FilterError;
use super::expr::UNRESOLVABLE_VALUE;

impl FilterColumns {
    /// The membership question `/v1/categories` asks of a `derived` column: which of this
    /// column's values does at least one entity in `candidate` carry (per-point-attributes §3.3)?
    ///
    /// **Derived, never maintained**, and evaluated entirely inside the composed verdict — so a
    /// value whose last visible member was suppressed stops being offered without a third
    /// retirement rule.
    ///
    /// **The postings are the base build's, so the extents are swept here, once.** A value carried
    /// only by entities ingested since the build must still be offered to a principal who can see
    /// one of them; deriving that per value would rescan the extents per value, so the sweep
    /// collects the codes the candidate's extent entities carry in a single pass and the per-value
    /// test is then a bitmap intersection against the postings plus a set lookup.
    pub fn category_membership<'a>(
        &'a self,
        column: &str,
        candidate: &'a Bitmap,
    ) -> Result<CategoryMembership<'a>, FilterError> {
        let held = self
            .columns
            .get(column)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        let layers = held.value_layers();
        // A column declared at a running service holds no base until the fold (`ingest.md`
        // §6.3): its layers are the flushes' extents, every member is in them, and the sweep
        // below is the whole answer. A column that holds a **base** layer (the one opened with no
        // `values_rel`) and no postings is one whose member sets cannot be read, since the base's
        // members are in the postings and nowhere else; answering from the extents alone would
        // offer a value to nobody who sees only its base members.
        let holds_base = layers.iter().any(|l| l.values_rel.is_none());
        let postings: Option<&ColumnPostings> = match held.postings() {
            Some(postings) => Some(postings),
            None if !holds_base => None,
            None => return Err(FilterError::MembershipUnavailable(column.to_string())),
        };

        // **A count per code, not a set of codes.** The sweep is the same one pass over the same
        // entities either way, and counting in it is what lets `?counts=true` be exact without a
        // second sweep: the extents and the base postings are disjoint in entity space — a posting
        // covers the base build and an extent covers entities ingested since it — so the two halves
        // of a value's count add rather than overlapping.
        let mut from_extents: FxHashMap<u32, u64> = FxHashMap::default();
        for layer in layers.iter().filter(|l| l.values_rel.is_some()) {
            layer.values.for_each_code_in(candidate, |code| {
                *from_extents.entry(code).or_default() += 1;
            });
        }
        // **The one thing a count may not assume**, checked where it is cheap rather than argued
        // where it is not: `intersection_cardinality` is exact per source and cardinality does not
        // distribute over a union, so a category column that ever acquired delta postings tiers
        // would need the materialising route. None does today — a flush writes extents for a
        // category, never postings (decision 0063) — and this is where that stops being an
        // assumption. `carries` is unaffected either way, existence *does* distribute.
        let postings_are_single_source = postings.is_none_or(|p| !p.has_tiers());
        Ok(CategoryMembership {
            column: column.to_string(),
            postings,
            candidate,
            from_extents,
            postings_are_single_source,
        })
    }
}

/// One column's value-visibility predicate for one principal, at one generation.
///
/// Built by [`FilterColumns::category_membership`]; see its doc for why the extents are swept up
/// front and the postings probed per value.
pub struct CategoryMembership<'a> {
    column: String,
    /// The base build's postings, or `None` for a column that has no base yet: one declared at a
    /// running service and not yet folded, whose every member is in `from_extents`.
    postings: Option<&'a ColumnPostings>,
    candidate: &'a Bitmap,
    /// The codes the candidate's *post-build* entities carry, **and how many of them carry each** —
    /// the half no posting covers. Disjoint from the postings' half in entity space, which is what
    /// lets [`Self::count`] add the two.
    from_extents: FxHashMap<u32, u64>,
    /// Whether the column's postings are one record per value rather than a base plus live tiers.
    /// [`Self::count`] refuses otherwise; see [`FilterColumns::category_membership`].
    postings_are_single_source: bool,
}

impl CategoryMembership<'_> {
    /// The column this predicate was built for, for a caller shaping a refusal that names it.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Is `code` carried by at least one entity this principal may see?
    ///
    /// The extent half is answered first because it is a hash lookup against a set the sweep
    /// already built, and because a value minted since the build has no posting at all — asking the
    /// postings first would be a file read per such value for an answer already in hand.
    pub fn carries(&self, code: u32) -> Result<bool, FilterError> {
        if code == UNRESOLVABLE_VALUE.raw() {
            // The reserved *absent* sentinel: never drawn, never bound to a key, and carried by
            // exactly the entities that carry no value. It is not a value and is never visible.
            return Ok(false);
        }
        if self.from_extents.contains_key(&code) {
            return Ok(true);
        }
        // **A boolean against the mapped view, never a materialised posting.** A value's posting is
        // corpus-wide — every entity carrying it, hidden ones included — and the question is one
        // bit, asked once per value walked. `ColumnPostings::intersects` short-circuits at the
        // first container the two sets share and allocates nothing.
        let Some(postings) = self.postings else {
            return Ok(false);
        };
        postings
            .intersects(AttrLocalId::new(code), self.candidate)
            .map_err(|e| FilterError::postings_unreadable(&self.column, e))
    }

    /// **How many items carrying `code` this principal may see** — C8's `and_cardinality` against
    /// the composed mask, exact, computed per request and never precomputed
    /// (`value-suggestion.md` §3).
    ///
    /// The two halves add because they are disjoint in entity space: the extents sweep counted the
    /// candidate's *post-build* entities and the postings cover the base build alone.
    ///
    /// Never a sort key. The count is the viewer's own number and would be admissible as one under
    /// **I2**, but a count-ordered page is a top-*k* over the prefix and depends on which values
    /// were examined before the budget ran out (§8.2, and decision 0069 for the corpus-global
    /// alternative). Ordering is the matched text's, and this is information beside a row.
    pub fn count(&self, code: u32) -> Result<u64, FilterError> {
        if code == UNRESOLVABLE_VALUE.raw() {
            return Ok(0);
        }
        if !self.postings_are_single_source {
            return Err(FilterError::MembershipUnavailable(self.column.clone()));
        }
        let extents = self.from_extents.get(&code).copied().unwrap_or(0);
        let base = match self.postings {
            None => 0,
            Some(postings) => postings
                .intersection_cardinality(AttrLocalId::new(code), self.candidate)
                .map_err(|e| FilterError::postings_unreadable(&self.column, e))?,
        };
        Ok(extents + base)
    }
}
