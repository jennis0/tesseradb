//! **The view gate** (`views.md` §6): which views and which groups a principal may reach.
//!
//! A view is a coordinate system over items that carry their own labels, and the ordinary corpus
//! gates none of them — so the whole of this module is about the corpus that does. Two labels
//! decide one view: the **group's**, which is the outer bound over every view of it, and the
//! **view's own**, taken as written inside it. They are conjunctive, so a view gate narrows its
//! group's and can never widen it — the relation decision 0089 gives an artifact to its layer,
//! and the I12 direction.
//!
//! **Satisfaction is intersection** (§6.1): the gate's labels resolve through the plugin to a set
//! of descriptors, one per label (decision 0132), and the gate is satisfied iff that set
//! **intersects** the descriptors the principal's credential resolved to. It is deliberately *not* the conservative label join's required-set
//! reading: under that reading a disjunctive gate (`["finance", "legal"]`) yields an empty
//! required set and every principal passes, which is a fail-open on exactly what the gate
//! protects. Intersection gives a disjunctive gate its intended meaning.
//!
//! **`public` never reaches the plugin.** It is stored as `None` — the absence of a gate — because
//! it is the one label every principal holds inside the trust boundary (decision 0088, term 0),
//! and asking caller-supplied code to confirm that would make the corpus's only universal label
//! depend on the plugin agreeing.
//!
//! **Resolved once, at authorise, over every view of every group, whatever the outcome.** The
//! result is the immutable [`VisibleViews`] a session carries for its whole life, so the
//! request-time check is one set-membership lookup and a gate-failed name costs the same work as a
//! name nobody ever declared — r23's work-indistinguishability standard, and the closure Appendix
//! C's C4 records for `/v1/items`. A view created after a session authorised is a 404 to that
//! session until it re-authorises (owner ruling 2026-08-30): the alternatives — a per-request gate
//! evaluation, or a lazily-evaluated miss — each cost exactly that property. Roster immutability
//! (`views.md` §3.2) is the other half: a gate, once written, never changes, so a fixed set can
//! never hold a stale *widening*.

use std::collections::HashMap;

use rustc_hash::FxHashSet;
use tessera_plugin::Plugin;
use tessera_store::manifest::Manifest;

/// The views and the groups one session may reach (`views.md` §6) — resolved at authorise and
/// never re-evaluated within the session.
///
/// **Two sets, because two surfaces ask different questions.** A viewer verb names a view; a
/// filter leaf naming a group-scoped attribute asks about the *group*, because for a principal
/// whose group gate fails the whole attribute is undeclared — bare and pinned uses alike take the
/// unknown-column refusal and nothing confirms the group or its keys (`views.md` §5). Deriving the
/// second from the first would answer *no view of it is reachable*, which is a different question
/// with the same answer only by accident.
#[derive(Debug, Clone, Default)]
pub struct VisibleViews {
    views: FxHashSet<String>,
    groups: FxHashSet<String>,
}

/// The probe made for a name that resolved to no view at all, so that the set lookup happens on
/// **both** outcomes and a gate-failed name and a never-declared one cost the same work. A view id
/// is never empty — `tessera_types::view::check_view_key` refuses it at every door — so this
/// probes the set and cannot hit.
pub(crate) const NO_SUCH_VIEW: &str = "";

impl VisibleViews {
    /// Is this view id (`ViewDescriptor::id` — a plain view's name or `<group>:<key>`, a view's
    /// only address) one this session may reach? One set-membership lookup, and the only question the request path asks
    /// of the gate.
    pub fn contains_view(&self, id: &str) -> bool {
        self.views.contains(id)
    }

    /// Is this group one this session may reach — its own gate, independent of whether any view of
    /// it survived its own?
    pub fn contains_group(&self, name: &str) -> bool {
        self.groups.contains(name)
    }

    /// How many views this session may reach. Operator-facing and test-facing; nothing on the wire
    /// carries it.
    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }
}

/// Evaluate every view of every group against a principal's credential descriptors (`views.md` §6).
///
/// **Every view is evaluated whatever the outcome**, which is what makes the answer a set rather
/// than a decision procedure: the cost is paid once, at authorise, in exchange for a request path
/// that never asks the plugin anything and never walks a roster.
///
/// **A label the plugin cannot read fails the view closed** rather than failing the whole
/// authorise. The build and the create operation both put a label through this same plugin call
/// before storing it, so this can only fire where a bundle is served under a *different* plugin
/// from the one that accepted its labels — and denying one view is the narrow answer to that,
/// where refusing the credential would deny a corpus its principal is otherwise entitled to.
pub(crate) fn resolve(
    manifest: &Manifest,
    credentials: &FxHashSet<Vec<u8>>,
    plugin: &dyn Plugin,
) -> VisibleViews {
    // One plugin call per **distinct gate**, not per view: a group of forty quarters under one
    // gate asks once. The memo is scoped to this resolution, so nothing survives into the session.
    let mut memo: HashMap<&[String], bool> = HashMap::new();

    let mut groups: FxHashSet<String> = FxHashSet::default();
    // Which group a view id belongs to, for the outer bound below. Built from the rosters rather
    // than by splitting the id on the separator: a plain view may carry no separator at all, and a
    // `members` group's views are named for the sharing group rather than for the owner whose keys
    // they are (`views.md` §3.3), so each of them takes *its own* group's gate.
    let mut owner: HashMap<String, &str> = HashMap::new();
    for group in &manifest.groups {
        if passes(
            &mut memo,
            group.visibility.as_deref(),
            credentials,
            plugin,
        ) {
            groups.insert(group.name.clone());
        }
        for view in &group.views {
            owner.insert(
                format!(
                    "{}{}{}",
                    group.name,
                    tessera_store::GROUP_SEPARATOR,
                    view.key
                ),
                group.name.as_str(),
            );
        }
    }

    let mut views: FxHashSet<String> = FxHashSet::default();
    for view in &manifest.views {
        // **The view's own half, from the descriptor.** For a view of a group this is the roster
        // record's own label, which `Manifest::validate_groups` holds equal to it — one input, so
        // a view's own gate is read the same way whether the view is a plain one or a group's.
        if !passes(
            &mut memo,
            view.visibility.as_deref(),
            credentials,
            plugin,
        ) {
            continue;
        }
        // **The group's half, where there is a group**: a gate-failed group takes its whole roster
        // with it.
        if let Some(group) = owner.get(view.id.as_str()) {
            if !groups.contains(*group) {
                continue;
            }
        }
        views.insert(view.id.clone());
    }
    VisibleViews { views, groups }
}

/// Does one of `labels` name a term this principal holds? `None` is `public` — satisfied by
/// construction, inside the trust boundary, and never through the plugin (decision 0088).
///
/// Each label is one element of the plugin's list call, never split: a gate declared as
/// `"finance,legal"` is one term with a comma in it, and a gate wanting both is the
/// two-element list. The descriptors are compared with the credential's own, as a layer's and an
/// artifact's are, so a label no item carries is still one a credential can hold.
fn passes<'a>(
    memo: &mut HashMap<&'a [String], bool>,
    labels: Option<&'a [String]>,
    credentials: &FxHashSet<Vec<u8>>,
    plugin: &dyn Plugin,
) -> bool {
    let Some(labels) = labels else { return true };
    if let Some(&known) = memo.get(labels) {
        return known;
    }
    let descriptors: Vec<Vec<u8>> = labels.iter().map(|l| l.as_bytes().to_vec()).collect();
    let verdict = match plugin.terms_of_labels(&descriptors) {
        // **Intersection, not the required set** — see this module's own doc for why the
        // conservative label join is fail-open on a disjunctive gate.
        Ok(descriptors) => descriptors.iter().any(|d| credentials.contains(d)),
        Err(_) => false,
    };
    memo.insert(labels, verdict);
    verdict
}
