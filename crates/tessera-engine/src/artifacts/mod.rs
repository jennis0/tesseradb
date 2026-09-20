//! Whether an artifact is served, and what number sits beside it.
//!
//! **One predicate, evaluated on every route.** The viewport, drill-down, filters, search, edge
//! traversal and metadata all call [`ArtifactView::verdict`] and nothing else. Where a route cannot
//! afford it, the route does not exist — that is what keeps the leak register exhaustive by
//! construction rather than by audit.
//!
//! The order of the conjuncts is not cosmetic:
//!
//! 0. **There is an artifact at that ordinal, in this view.** A level's row form is built for one
//!    view of one layer, so a hole, an ordinal past its end and a group-scoped layer's artifact
//!    belonging to another view of the group are one answer: absent ([`ArtifactRows::holds`],
//!    `views.md` §3.5). The ordinal is an address over the whole layer and the view is part of a
//!    scoped artifact's identity, so this is where the two are reconciled. Without it a view of a
//!    group-scoped layer serves the group's other views' artifacts — their keys, their
//!    identifiers, and a masked count of zero, which a layer declaring no existence criterion has
//!    nothing to withhold.
//! 1. **The overlay, first and unconditional.** A suppression applies to every request the moment
//!    it is accepted, whatever else is true, so an artifact reaches the same `deleted > suppressed`
//!    composition a point does, by the same route.
//! 2. **The layer's gate.** Whether this viewer may know the layer exists at all.
//! 3. **The artifact it depends on, if it depends on one — visible to this viewer, entire.** An
//!    artifact published as an attachment to another — a toponymy label on a cluster — is served
//!    only where the artifact it attaches to is served
//!    ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
//!    rule 2). Without that term the predicate is per-artifact by construction, so suppressing a
//!    cluster stops the cluster serving while every label naming and describing it goes on serving
//!    to whoever reaches it directly: by search, by a held identifier, by a filter. The model's
//!    conjunctive rule covers edge *traversal* and those routes traverse nothing
//!    (`annotation-representation.md` §4).
//!
//!    **The target's whole predicate, and per artifact rather than per layer.** The term was once
//!    three cheaper ones — the target's disposition, its layer's reachability, and whether its slot
//!    still exists — which left a target withheld by *its own* criterion still nameable by a label
//!    on a layer declaring a weaker one
//!    ([decision 0086](../../../docs/decisions/0086-the-attachment-term-does-not-inherit-the-targets-criterion.md),
//!    superseded on this point by 0089). Rule 2 closes it: the prerequisite is the same `verdict`
//!    call evaluated for the target's layer, so the criterion, the own-terms gate and containment
//!    all count, and the conjunction can only narrow what a principal sees. It costs the target's
//!    masked count per attached artifact per request, which is what 0086 declined to pay and 0089
//!    rules is paid.
//!
//!    Existence is inside that call rather than beside it, and the fold is why it has to be asked
//!    at all: an overlay entry says *deleted*, and the fold that executes the deletion retires the
//!    entry in the same publication that drops the target's slot — so a term resting on the overlay
//!    alone would start serving every label attached to a deleted cluster at the next nightly fold.
//!    A hole answers *not served*, which makes the fold's own hole the durable form of the
//!    withholding rather than a state something has to remember.
//!    What it attaches to is also where its membership comes from, where it declared none
//!    (decision 0145). A label with no member rows is the label of its cluster. It is placed where
//!    the cluster is placed and counted over the cluster's members, so the number below and the
//!    tile above read the target's membership. [`ArtifactRows::inherit`] resolves that once, for
//!    every route.
//! 4. **The artifact's own terms, if its layer declared that its artifacts carry them.**
//! 5. **The existence criterion, if declared** — the masked count against a declared bar.
//!
//! Two of those were once one thing, and separating them is
//! [decision 0079](../../../docs/decisions/0079-the-gate-is-one-flag-not-three-modes.md): the three
//! gate modes it replaced were a two-by-two in three names, and *substitutive* switched the
//! criterion off, so a corpus-derived clustering mis-declared served the existence and count of
//! every cluster down to a single member. Under a flag beside an independent criterion, one schema
//! word can no longer disable a disclosure control.
//!
//! ## The count is masked, and the criterion never touches it
//!
//! `|rows(artifact) ∩ M|` where `M` is the session's **composed** mask — its projection with the
//! overlay's denials taken out and the buffer's additions put in, both operands already row-space.
//! The type enforces that: [`MaskedSet`](crate::compose::MaskedSet) has exactly one implementor outside a test build, so a
//! count cannot be taken against the pre-overlay projection, which strictly contains `M_auth` after
//! any accepted delete. That number is what a viewer is told, unmodified. The criterion
//! reads the same number and decides whether the artifact is **served at all**
//! ([decision 0075](../../../docs/decisions/0075-the-masked-count-is-an-existence-criterion.md));
//! it never rounds, floors or suppresses a value. An implementation that "applied the threshold to
//! the count" would be a different design with a different disclosure.
//!
//! **A below-criterion artifact is absent, not refused.** It does not appear, and the response
//! carries nothing that distinguishes it from an artifact that was never published — which is the
//! same indistinguishability the layer registry gives a gate-failed name.
//!
//! ## The row form is maintained, not invalidated
//!
//! A level's [`ArtifactRows`] covers the **whole** of its view's row space — base rows and every
//! flushed extent — and it is brought forward by the operations that change it rather than rebuilt
//! by them:
//!
//! - a **growth** or a **publication** applies its own delta to every held form of that level
//!   ([`ArtifactProjections::bring_forward`]) and moves the form's key with it, so the next request
//!   hits;
//! - a **flush** extends every held form of the view by the segment it published
//!   ([`ArtifactProjections::extend_flushed`]), which is what makes an ingested member count from
//!   its flush rather than from the next fold.
//!
//! - a **merge** rebases every held form of the view over the extent it published
//!   ([`ArtifactProjections::rebase_merged`]): the rows inside the merged span are cleared and
//!   the members re-projected through the one merged extent. Extent rows are the rows a merge
//!   renumbers, and this is the one publication that permutes rows a form holds.
//!
//! All three re-derive the tile index and amend the row-major column in place, because both are
//! pure functions of the form and the fold's own files describe the level as it was (**I11**:
//! nothing persisted is amended, and nothing persisted is reused past what it describes). All are
//! per `(view, layer, level)` and derived from the level's records and the row space, which is what
//! keeps them the same shared structure a built form is (**I2**).
//!
//! **A spatial level's form is the same form with another source of rows.** Its membership is
//! resolved from the level's shapes rather than projected from records (`crate::shapes`), so the
//! rows a flush or a merge brings are the segment's resolution and the rows a publication brings
//! are the new shapes' resolution over every live segment ([`SegmentRows::Resolved`],
//! [`DeltaRows::Resolved`]); everything from the union on is shared with a stored level. An
//! attribute predicate's form is the exception: its membership is the value column, evaluated per
//! request, so it takes no delta and is keyed on the geometry ([`ProjectionKey::live`]).
//!
//! **A content's generating set is projected over the whole row space**, as a membership is, and
//! the same three amendments maintain it: a flush extends it by the segment's extent, a merge
//! rebases it over the merged span, and a growth unions the entities a page joined. A member still
//! in the commit buffer has no row, so the set is short until the flush that gives it one and the
//! content is withheld meanwhile — [`MembershipRows::put`] has the argument.
//!
//! **[`ArtifactRows::covers`] is read at every cache hit** because a request may hold the older of
//! two live generations; on the executor every publication brings the held forms with it, so a
//! form that was current cannot fail it there. Before 2026-09-03 the form held base rows alone and
//! needed no such check; it also understated every count by the members ingested since the last
//! fold, and rebuilt the level whole — 94 to 177 s at rung 3, inside a request — at every write
//! that moved the level's version (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`).
//! Until 2026-09-06 a merge dropped the form and the next request paid the same projection.


mod projections;
mod projections_build;
mod projections_forward;
mod rows;
mod rows_build;
mod rows_forward;
mod sources;
#[cfg(test)]
mod test_support;
mod view;

pub use projections::{
    derives_accumulated_geometry, serves_column_only, ArtifactProjections, DeltaKind, DeltaRows,
    LevelDelta, SegmentRows, SetPage, ROW_COLUMN_SCRATCH_DIR,
};
pub use rows::{
    ArtifactRecords, ArtifactRows, Candidacy, Containment, Matched, MembershipRows,
};
pub use sources::{AttributeSource, PredicateSource, SpatialSource};
pub use view::{ArtifactVerdict, ArtifactView, Withheld};

pub(crate) use rows::view_key;

use projections::{Coordinate, Held, LevelAddress, ProjectionKey};
use rows::{covered_by, drawn_record, total_rows};
