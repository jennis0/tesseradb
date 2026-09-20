use croaring::Bitmap;
use rustc_hash::FxHashMap;
use tessera_filter::ColumnPostings;
use tessera_types::AttrLocalId;

use super::columns::FilterColumns;
use super::error::FilterError;
use super::expr::UNRESOLVABLE_VALUE;

impl FilterColumns {
    /// The membership question `/v1/categories` asks of a `derived` column: which of this
    /// column's values does at least one entity in `candidate` carry? Derived, never maintained,
    /// and evaluated entirely inside the composed verdict.
    ///
    /// The postings are the base build's, so the extents are swept here, once, collecting the
    /// codes the candidate's extent entities carry; the per-value test is then a bitmap
    /// intersection against the postings plus a set lookup.
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
        // A column with no base holds only extents, so the sweep is the whole answer. A base
        // layer with no postings has no member set to read.
        let holds_base = layers.iter().any(|l| l.values_rel.is_none());
        let postings: Option<&ColumnPostings> = match held.postings() {
            Some(postings) => Some(postings),
            None if !holds_base => None,
            None => return Err(FilterError::MembershipUnavailable(column.to_string())),
        };

        // Counted in the same pass: extents and base postings are disjoint, so the two halves add.
        let mut from_extents: FxHashMap<u32, u64> = FxHashMap::default();
        for layer in layers.iter().filter(|l| l.values_rel.is_some()) {
            layer.values.for_each_code_in(candidate, |code| {
                *from_extents.entry(code).or_default() += 1;
            });
        }
        // Cardinality does not distribute over a union, so a category column with postings tiers
        // would need the materialising route; `carries` is unaffected, since existence does.
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
    /// The base build's postings, `None` for a column with no base yet.
    postings: Option<&'a ColumnPostings>,
    candidate: &'a Bitmap,
    /// The codes the candidate's post-build entities carry, and how many, the half no posting
    /// covers, disjoint from the postings' half.
    from_extents: FxHashMap<u32, u64>,
    /// Whether the column's postings are one record per value rather than a base plus live tiers.
    /// [`Self::count`] refuses otherwise.
    postings_are_single_source: bool,
}

impl CategoryMembership<'_> {
    /// The column this predicate was built for, for a caller shaping a refusal that names it.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Is `code` carried by at least one entity this principal may see? The extent half is
    /// answered first, a hash lookup already in hand.
    pub fn carries(&self, code: u32) -> Result<bool, FilterError> {
        if code == UNRESOLVABLE_VALUE.raw() {
            // The reserved absent sentinel, carried by exactly the entities that carry no value:
            // it is not a value and is never visible.
            return Ok(false);
        }
        if self.from_extents.contains_key(&code) {
            return Ok(true);
        }
        // A boolean against the mapped view, never a materialised posting: `intersects`
        // short-circuits at the first shared container and allocates nothing.
        let Some(postings) = self.postings else {
            return Ok(false);
        };
        postings
            .intersects(AttrLocalId::new(code), self.candidate)
            .map_err(|e| FilterError::postings_unreadable(&self.column, e))
    }

    /// How many items carrying `code` this principal may see: an exact intersection against the
    /// composed mask, computed per request. The two halves add because they are disjoint in
    /// entity space. Never a sort key: a count-ordered page would be a top-*k* over the prefix,
    /// depending on which values were examined before the budget ran out.
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
