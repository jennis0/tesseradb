//! The view roster: which views of which groups exist, at which incarnation, and which
//! incarnations are dead (`views.md` §3.2, §3.4;
//! [decision 0108](../../../docs/decisions/0108-a-view-group-grows-by-its-roster.md),
//! [decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)).
//!
//! Beside [`crate::registry`] and shaped like it, because the two problems are the same one: a
//! named object created while the service runs, made durable by a WAL record and carried forward
//! by the segments manifest. What differs is what a name costs — a layer's name carries entities and a view's carries a coordinate system — and
//! what each is measured against: a layer against its own declaration, a view against the group's.
//!
//! **What lives here is the runtime half.** The views a build declared are in `MANIFEST.json` and
//! are seeded in ([`ViewRoster::seed_declared`]) so that their keys are taken; what this structure
//! *owns* is the creations and the tombstones, which is exactly what a publication writes and a
//! replay reads back.
//!
//! **Three rules the operations here exist to hold:**
//!
//! - **A roster record is immutable.** There is no update: an existing key is refused, and a wrong
//!   record is a drop and a recreate under a new key. The alternative is a narrowed gate that does
//!   not bite live sessions, a staleness the deny lane is not allowed and the roster is not either.
//! - **A dropped key is reusable, and a recreate mints a new incarnation** (decision 0115). A key
//!   is a human-chosen name, not a system identity, and the correction workflow the immutability
//!   rule above forces — a wrong record is a drop and a recreate — is only useful if the name can
//!   come back. What must never come back is the *predecessor's artifacts*: they linger until the
//!   fold reclaims them, so the create mints an incarnation, every artifact carries the one it was
//!   written under, and composition takes only the live one. The incarnation is internal: no wire
//!   surface carries it and no client can tell a recreated key from a fresh one.
//! - **A key belongs to the group that owns it.** A group declaring `members` takes
//!   another group's (`views.md` §3.3), so a create names the owner and the sharing groups' copies
//!   are derived from that one record. Two records would be two places for them to disagree.

use std::collections::BTreeMap;

use tessera_types::view::{
    check_view_key, CreatedView, DeadIncarnation, GroupMetadataField, ViewIncarnation,
    ViewMetadataType, ViewMetadataValue, DECLARED_INCARNATION,
};

use crate::wal::WalRecord;

/// Why a create or a drop was refused. Every variant is the caller's own request measured against
/// state only the executor may read, which is why none of them is checkable in a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterError {
    /// The key is already a view of this group.
    Exists { group: String, key: String },
    /// No view of this group holds the key, so there is nothing to drop.
    Unknown { group: String, key: String },
    /// The request's own shape: the key's charset, the metadata, or the gate.
    Refused(String),
}

impl std::fmt::Display for RosterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RosterError::Exists { group, key } => write!(
                f,
                "view '{group}:{key}' already exists. A roster record is immutable (views §3.2, \
                 decision 0108): a wrong gate or wrong metadata is a drop and a recreate under a \
                 NEW key, never an update, because an updatable gate is a narrowing that does not \
                 bite live sessions"
            ),
            RosterError::Unknown { group, key } => {
                write!(f, "no view '{group}:{key}' — nothing to drop")
            }
            RosterError::Refused(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for RosterError {}

/// What the group a create names declares, as the executor reads it off the manifest.
///
/// Borrowed rather than owned because it is read inside the one lock the preparation holds, and
/// because the manifest is the authority: a copy kept here would be a second declaration to keep
/// in step with the first.
#[derive(Debug, Clone, Copy)]
pub struct GroupFacts<'a> {
    pub name: &'a str,
    /// The group whose keys these are, where this group declares `members`; `None` where it owns
    /// them.
    pub members_of: Option<&'a str>,
    /// The per-view metadata names and types this group declared.
    pub metadata: &'a [GroupMetadataField],
}

/// The roster a create and a drop operate on.
#[derive(Debug, Default, Clone)]
pub struct ViewRoster {
    /// The creations, in creation order — what a publication writes and a replay reads back.
    created: Vec<CreatedView>,
    /// Every incarnation that has died and whose artifacts the fold has not yet reclaimed.
    dead: Vec<DeadIncarnation>,
    /// `(group, key) -> incarnation` for every view that exists right now, the build's included
    /// (at [`DECLARED_INCARNATION`]), so an existing key is one lookup rather than a walk of two
    /// lists and a subtraction.
    live: BTreeMap<(String, String), ViewIncarnation>,
    /// The next incarnation to mint — one above every incarnation this roster has ever seen, live
    /// or dead. Seeded from durable state and moved by every record applied, so a replay never
    /// mints a value a record already spent.
    next_incarnation: ViewIncarnation,
}

impl ViewRoster {
    pub fn new() -> Self {
        ViewRoster::default()
    }

    /// Seed the views a **build** declared: their keys are taken.
    ///
    /// Called before [`Self::seed`] and before any WAL record. A group's declared views are read
    /// off `MANIFEST.json`, which is the roster's other half and the half that never changes.
    pub fn seed_declared(&mut self, views: impl IntoIterator<Item = (String, String)>) {
        for (group, key) in views {
            // **The mint's floor moves for these too**, so the seed genuinely covers every
            // incarnation the manifests carry: a declared key that is dropped and created again
            // must not be handed the number its build segments are stamped with (decision 0115).
            self.next_incarnation = self.next_incarnation.max(DECLARED_INCARNATION + 1);
            self.live.insert((group, key), DECLARED_INCARNATION);
        }
    }

    /// Seed the runtime half from a segments manifest — the creations and the tombstones, exactly
    /// as they were published.
    ///
    /// **A tombstone is applied after the creation it retires**, whether or not that creation is
    /// still in the log: the two lists are complete current state, not a diff, so a key in both is
    /// a key that was created and then dropped.
    /// **The dead list is applied after the creations it retires, and a creation of a *later*
    /// incarnation survives it**: the two lists are complete current state, not a diff, so a key
    /// in both is a key that was created, dropped, and — if the creation names the higher
    /// incarnation — created again.
    pub fn seed(&mut self, created: &[CreatedView], dead: &[DeadIncarnation]) {
        for view in created {
            self.admit(view.clone());
        }
        for stone in dead {
            self.retire(stone.clone());
        }
    }

    fn admit(&mut self, view: CreatedView) {
        self.next_incarnation = self.next_incarnation.max(view.incarnation + 1);
        self.live
            .insert((view.group.clone(), view.key.clone()), view.incarnation);
        if let Some(existing) = self
            .created
            .iter_mut()
            .find(|v| v.group == view.group && v.key == view.key)
        {
            // A recreate under the same key replaces the record in place — it is one row of the
            // roster, at whichever incarnation is current, and creation order is the order the key
            // was *first* served in.
            *existing = view;
        } else {
            self.created.push(view);
        }
    }

    fn retire(&mut self, stone: DeadIncarnation) {
        self.next_incarnation = self.next_incarnation.max(stone.incarnation + 1);
        // **Only the incarnation named**, so a seed that meets a drop and a later recreate in
        // either order lands on the same state: a live record of a *higher* incarnation is not
        // touched by a death below it.
        if self.live.get(&(stone.group.clone(), stone.key.clone())) == Some(&stone.incarnation) {
            self.live.remove(&(stone.group.clone(), stone.key.clone()));
            self.created
                .retain(|v| !(v.group == stone.group && v.key == stone.key));
        }
        if !self.dead.iter().any(|s| {
            s.group == stone.group && s.key == stone.key && s.incarnation == stone.incarnation
        }) {
            self.dead.push(stone);
        }
    }

    /// The views created since the build, in creation order.
    pub fn created(&self) -> &[CreatedView] {
        &self.created
    }

    /// Every incarnation that has died and not yet been reclaimed.
    pub fn dead_incarnations(&self) -> &[DeadIncarnation] {
        &self.dead
    }

    /// Is this key a view of this group right now?
    pub fn is_live(&self, group: &str, key: &str) -> bool {
        self.live
            .contains_key(&(group.to_string(), key.to_string()))
    }

    /// Which incarnation this key is at right now, or `None` if it is not a view of this group.
    ///
    /// **The one resolution site, and it fails closed**: a caller that cannot resolve an
    /// incarnation must treat the artifact as unreachable, never as live.
    pub fn incarnation_of(&self, group: &str, key: &str) -> Option<ViewIncarnation> {
        self.live
            .get(&(group.to_string(), key.to_string()))
            .copied()
    }

    /// What a publication carries forward — complete current state, never a diff, on the posture
    /// the segments manifest already takes for `deny`, `tombstones` and `layers`.
    pub fn snapshot(&self) -> (Vec<CreatedView>, Vec<DeadIncarnation>) {
        (self.created.clone(), self.dead.clone())
    }

    /// The record a `PUT /control/views/{group}/{key}` appends, or the refusal.
    ///
    /// **Prepares only: nothing here mutates the roster.** The record is applied by
    /// [`Self::apply`] once it is durable, which is the same order the layer registry takes and
    /// for the same reason — a name handed out before its record is on disc comes back from a
    /// restart as a free name, having already been acked.
    pub fn prepare_create(
        &self,
        facts: GroupFacts<'_>,
        key: &str,
        visibility: Option<Vec<String>>,
        metadata: BTreeMap<String, ViewMetadataValue>,
    ) -> Result<WalRecord, RosterError> {
        if let Some(owner) = facts.members_of {
            return Err(RosterError::Refused(format!(
                "view group '{}' takes its views from group '{owner}' (views §3.3), so a key is \
                 created on '{owner}' and appears here at the same moment. Keys and metadata \
                 belong to the group that owns them",
                facts.name
            )));
        }
        check_view_key(key).map_err(RosterError::Refused)?;
        // **A dropped key is not refused** (decision 0115). What was a `409` on a tombstone is a
        // `201` at a fresh incarnation: the key is a name the caller chose, and the predecessor's
        // artifacts are kept out by the incarnation below, not by refusing the name.
        if self.is_live(facts.name, key) {
            return Err(RosterError::Exists {
                group: facts.name.to_string(),
                key: key.to_string(),
            });
        }
        // **`public` compiles to no gate** (`views.md` §6, decision 0088): it is the label every
        // principal holds inside the trust boundary, so the word and the absence are one
        // statement and the record keeps the shorter of them — which is also what makes a stored
        // label always a term to look up rather than sometimes the reserved word.
        //
        // A real label is **not checked here**: whether the plugin can read it is a question only
        // the engine can ask, and `Engine::create_view` asks it before this record is prepared, on
        // the same route an item's `access` labels take at ingest. The gate is a list of labels,
        // each one term (decision 0132); `public` is recognised only as the whole of the list.
        let visibility = visibility.filter(|labels| !tessera_types::label::is_public_gate(labels));
        // **A `timestamp_us` arrives as an integer, and the declaration is what says so.** JSON
        // carries no date type, so a record's `starts` is microseconds since the epoch as a
        // number; typing it from the wire alone would make every timestamp an `int` and refuse
        // every create. Only this one coercion exists — an integer is *not* accepted where a float
        // is declared, because those two are told apart in every other surface too.
        let mut metadata = metadata;
        for field in facts.metadata {
            if field.ty != ViewMetadataType::TimestampUs {
                continue;
            }
            if let Some(ViewMetadataValue::Int(v)) = metadata.get(&field.name) {
                let micros = *v;
                metadata.insert(field.name.clone(), ViewMetadataValue::TimestampUs(micros));
            }
        }
        // Every declared name, and no others. A missing one is refused rather than defaulted: the
        // record is immutable, so a value left out is a value that can never be supplied, and a
        // client reading the roster would see a name the group declares and this view does not
        // carry.
        for field in facts.metadata {
            // ⊘ **A category-typed metadata name has no create that can satisfy it**
            // (`views.md` §3.2). A category's value is a key resolved to its vocabulary's code,
            // and nothing on this path resolves one — accepting the integer instead would make
            // the caller the minting authority for a code space the server owns
            // (`per-point-attributes.md` §3.1), which is what a category column refuses on every
            // other surface. Refused by name rather than stored unresolved.
            if field.ty == ViewMetadataType::Category {
                return Err(RosterError::Refused(format!(
                    "view group '{}' declares metadata '{}' as a category, and a view of it \
                     cannot be created while the service runs: a category's value is a key \
                     resolved to its vocabulary's code, and codes are the server's to assign. \
                     Declare the name as a scalar, or add the view at a build",
                    facts.name, field.name
                )));
            }
            let Some(value) = metadata.get(&field.name) else {
                return Err(RosterError::Refused(format!(
                    "view group '{}' declares metadata '{}' ({}) and this record carries none. \
                     Every declared name is required: a roster record is immutable, so a value \
                     left out is one that can never be supplied (views §3.2)",
                    facts.name,
                    field.name,
                    field.ty.name()
                )));
            };
            if !field.ty.admits(value) {
                return Err(RosterError::Refused(format!(
                    "metadata '{}' is {}, and view group '{}' declares it {}",
                    field.name,
                    value.type_name(),
                    facts.name,
                    field.ty.name()
                )));
            }
        }
        for name in metadata.keys() {
            if !facts.metadata.iter().any(|f| &f.name == name) {
                return Err(RosterError::Refused(format!(
                    "metadata '{name}' is not declared by view group '{}'. Declared: {}. An \
                     undeclared name is refused rather than stored, because the roster is served \
                     typed and nothing would say what its type is",
                    facts.name,
                    if facts.metadata.is_empty() {
                        "none".to_string()
                    } else {
                        facts
                            .metadata
                            .iter()
                            .map(|f| f.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            }
        }
        Ok(WalRecord::ViewCreate {
            view: CreatedView {
                group: facts.name.to_string(),
                key: key.to_string(),
                // **Minted here and recorded**, never re-derived at replay: a re-derivation would
                // hand a recreate the incarnation its predecessor's artifacts already carry.
                incarnation: self.next_incarnation,
                visibility,
                metadata,
            },
        })
    }

    /// The record a `DELETE /control/views/{group}/{key}` appends, or the refusal.
    pub fn prepare_drop(&self, group: &str, key: &str) -> Result<WalRecord, RosterError> {
        let Some(incarnation) = self.incarnation_of(group, key) else {
            // A dropped key and a key that never existed are the same answer, and deliberately:
            // the drop's own 404 is what a request naming the view gets from then on.
            return Err(RosterError::Unknown {
                group: group.to_string(),
                key: key.to_string(),
            });
        };
        // **The incarnation travels with the drop**, because that is what makes the record
        // self-sufficient: replay may meet it without its own create (rotation), and a death that
        // did not say *which* incarnation died could not be told apart from a death of the one
        // created after it.
        Ok(WalRecord::ViewDrop {
            view: DeadIncarnation {
                group: group.to_string(),
                key: key.to_string(),
                incarnation,
            },
        })
    }

    /// Apply a durable record — at replay, and immediately after the append that made it durable.
    ///
    /// **A drop applies whether or not this log carried its create**, exactly as a layer drop
    /// does: a tombstone that depended on seeing its own create would evaporate at the first
    /// rotation, which is a key coming back to life.
    pub fn apply(&mut self, record: &WalRecord) {
        match record {
            WalRecord::ViewCreate { view } => self.admit(view.clone()),
            WalRecord::ViewDrop { view } => self.retire(view.clone()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(name: &'a str, metadata: &'a [GroupMetadataField]) -> GroupFacts<'a> {
        GroupFacts {
            name,
            members_of: None,
            metadata,
        }
    }

    fn create(roster: &mut ViewRoster, group: &str, key: &str) -> Result<(), RosterError> {
        let record = roster.prepare_create(facts(group, &[]), key, None, BTreeMap::new())?;
        roster.apply(&record);
        Ok(())
    }

    #[test]
    fn creations_are_kept_in_creation_order_past_the_build_and_past_a_drop() {
        let mut roster = ViewRoster::new();
        roster.seed_declared([("quarter".to_string(), "2026-Q1".to_string())]);
        create(&mut roster, "quarter", "2026-Q2").unwrap();
        create(&mut roster, "quarter", "2026-Q3").unwrap();

        // A drop removes its own record and leaves the rest in the order they were made: creation
        // order is what `/v1/meta` serves, and there is no number to re-derive (decision 0113).
        let drop = roster.prepare_drop("quarter", "2026-Q2").unwrap();
        roster.apply(&drop);
        create(&mut roster, "quarter", "2026-Q4").unwrap();
        let order: Vec<&str> = roster.created().iter().map(|v| v.key.as_str()).collect();
        assert_eq!(order, ["2026-Q3", "2026-Q4"]);
    }

    /// A live key is refused; a **dropped** key is not (decision 0115), and it comes back at a
    /// new incarnation so that nothing of its predecessor's is reachable under it.
    #[test]
    fn a_live_key_is_refused_and_a_dropped_one_comes_back_at_a_new_incarnation() {
        let mut roster = ViewRoster::new();
        create(&mut roster, "quarter", "2026-Q2").unwrap();
        let first = roster.incarnation_of("quarter", "2026-Q2").unwrap();
        assert!(matches!(
            create(&mut roster, "quarter", "2026-Q2"),
            Err(RosterError::Exists { .. })
        ));
        let drop = roster.prepare_drop("quarter", "2026-Q2").unwrap();
        roster.apply(&drop);
        assert!(matches!(
            roster.prepare_drop("quarter", "2026-Q2"),
            Err(RosterError::Unknown { .. })
        ));
        assert_eq!(roster.incarnation_of("quarter", "2026-Q2"), None);
        assert_eq!(
            roster.dead_incarnations(),
            [DeadIncarnation {
                group: "quarter".to_string(),
                key: "2026-Q2".to_string(),
                incarnation: first,
            }]
        );

        create(&mut roster, "quarter", "2026-Q2").expect("a dropped key is reusable");
        let second = roster.incarnation_of("quarter", "2026-Q2").unwrap();
        assert!(
            second > first,
            "the recreate must not reuse the incarnation its predecessor's artifacts carry"
        );
        // And the death of the first stays on the books: the fold reads it to know what it may
        // reclaim, and the composition reads it to keep the old artifacts out.
        assert_eq!(roster.dead_incarnations().len(), 1);
    }

    /// The seed is complete current state, and the two lists may be read in either order: a
    /// recreate's live record must survive a death recorded *below* it.
    #[test]
    fn a_death_below_the_live_incarnation_does_not_retire_it() {
        let mut roster = ViewRoster::new();
        roster.seed(
            &[CreatedView {
                group: "quarter".to_string(),
                key: "2026-Q2".to_string(),
                incarnation: 7,
                visibility: None,
                metadata: BTreeMap::new(),
            }],
            &[DeadIncarnation {
                group: "quarter".to_string(),
                key: "2026-Q2".to_string(),
                incarnation: 4,
            }],
        );
        assert_eq!(roster.incarnation_of("quarter", "2026-Q2"), Some(7));
        // And the next mint is above everything ever seen, dead included.
        let record = roster
            .prepare_create(facts("quarter", &[]), "2026-Q3", None, BTreeMap::new())
            .unwrap();
        match record {
            WalRecord::ViewCreate { view } => assert!(view.incarnation > 7),
            other => panic!("expected a create, got {other:?}"),
        }
    }

    #[test]
    fn a_drop_replays_without_its_own_create() {
        // The rotation case: the create's record is gone and the drop must still bite. A
        // build-declared key is dropped here, so the death is of `DECLARED_INCARNATION`.
        let mut roster = ViewRoster::new();
        roster.seed_declared([("quarter".to_string(), "2026-Q1".to_string())]);
        roster.apply(&WalRecord::ViewDrop {
            view: DeadIncarnation {
                group: "quarter".to_string(),
                key: "2026-Q1".to_string(),
                incarnation: DECLARED_INCARNATION,
            },
        });
        assert!(!roster.is_live("quarter", "2026-Q1"));
        // And the key is free again, at an incarnation above the build's.
        create(&mut roster, "quarter", "2026-Q1").expect("a dropped key is reusable");
        assert!(roster.incarnation_of("quarter", "2026-Q1").unwrap() > DECLARED_INCARNATION);
    }

    #[test]
    fn a_members_group_takes_no_create_of_its_own() {
        let roster = ViewRoster::new();
        let refusal = roster
            .prepare_create(
                GroupFacts {
                    name: "quarter_map",
                    members_of: Some("quarter"),
                    metadata: &[],
                },
                "2026-Q5",
                None,
                BTreeMap::new(),
            )
            .unwrap_err();
        assert!(matches!(refusal, RosterError::Refused(_)));
    }

    #[test]
    fn every_declared_metadata_name_is_required_and_typed() {
        let declared = [GroupMetadataField {
            name: "label".to_string(),
            ty: ViewMetadataType::Text,
            vocabulary: None,
        }];
        let roster = ViewRoster::new();
        assert!(roster
            .prepare_create(facts("quarter", &declared), "k", None, BTreeMap::new())
            .is_err());
        assert!(roster
            .prepare_create(
                facts("quarter", &declared),
                "k",
                None,
                [("label".to_string(), ViewMetadataValue::Int(3))]
                    .into_iter()
                    .collect(),
            )
            .is_err());
        assert!(roster
            .prepare_create(
                facts("quarter", &declared),
                "k",
                None,
                [(
                    "label".to_string(),
                    ViewMetadataValue::Text("Q5".to_string())
                )]
                .into_iter()
                .collect(),
            )
            .is_ok());
        // An undeclared name is refused rather than stored.
        assert!(roster
            .prepare_create(
                facts("quarter", &declared),
                "k",
                None,
                [
                    (
                        "label".to_string(),
                        ViewMetadataValue::Text("Q5".to_string())
                    ),
                    ("ends".to_string(), ViewMetadataValue::Int(1)),
                ]
                .into_iter()
                .collect(),
            )
            .is_err());
    }

    #[test]
    fn a_category_metadata_name_has_no_create_that_can_satisfy_it() {
        let declared = [GroupMetadataField {
            name: "tier".to_string(),
            ty: ViewMetadataType::Category,
            vocabulary: Some("tiers".to_string()),
        }];
        let roster = ViewRoster::new();
        // Not even with the code the manifest would store: a caller supplying one would be the
        // minting authority for a space the server owns.
        assert!(roster
            .prepare_create(
                facts("quarter", &declared),
                "k",
                None,
                [("tier".to_string(), ViewMetadataValue::Int(3))]
                    .into_iter()
                    .collect(),
            )
            .is_err());
    }

    #[test]
    fn a_timestamp_arrives_as_an_integer_and_a_float_is_still_a_float() {
        let declared = [
            GroupMetadataField {
                name: "starts".to_string(),
                ty: ViewMetadataType::TimestampUs,
                vocabulary: None,
            },
            GroupMetadataField {
                name: "weight".to_string(),
                ty: ViewMetadataType::Float,
                vocabulary: None,
            },
        ];
        let roster = ViewRoster::new();
        let record = roster
            .prepare_create(
                facts("quarter", &declared),
                "2026-Q5",
                None,
                [
                    ("starts".to_string(), ViewMetadataValue::Int(1_767_225_600)),
                    ("weight".to_string(), ViewMetadataValue::Float(0.5)),
                ]
                .into_iter()
                .collect(),
            )
            .expect("an integer is what a JSON timestamp is");
        match record {
            WalRecord::ViewCreate { view } => assert_eq!(
                view.metadata.get("starts"),
                Some(&ViewMetadataValue::TimestampUs(1_767_225_600)),
                "the declaration is what types it"
            ),
            _ => unreachable!(),
        }
        // The one coercion, and no other: an integer where a float is declared stays a refusal.
        assert!(roster
            .prepare_create(
                facts("quarter", &declared),
                "2026-Q6",
                None,
                [
                    ("starts".to_string(), ViewMetadataValue::Int(1)),
                    ("weight".to_string(), ViewMetadataValue::Int(1)),
                ]
                .into_iter()
                .collect(),
            )
            .is_err());
    }

    /// **`public` and no gate are one statement, and the record keeps the shorter** (`views.md`
    /// §6, decision 0088): every principal holds the label inside the trust boundary, so a record
    /// storing the word would make the roster's `visibility` sometimes a term to look up and
    /// sometimes a reserved one. A real label list is stored as written, each element one label
    /// (decision 0132); whether the plugin can read it is `Engine::create_view`'s question, this
    /// crate holding no plugin.
    #[test]
    fn public_is_recorded_as_no_gate_and_a_label_is_recorded_as_written() {
        let roster = ViewRoster::new();
        let gate_of = |declared: Option<&[&str]>| {
            let record = roster
                .prepare_create(
                    facts("quarter", &[]),
                    "k",
                    declared.map(|labels| labels.iter().map(|l| l.to_string()).collect()),
                    BTreeMap::new(),
                )
                .expect("a well-formed record");
            match record {
                WalRecord::ViewCreate { view } => view.visibility,
                other => panic!("expected a create record, got {other:?}"),
            }
        };
        assert_eq!(
            gate_of(Some(&["finance"])),
            Some(vec!["finance".to_string()])
        );
        assert_eq!(
            gate_of(Some(&["finance,legal", "tax"])),
            Some(vec!["finance,legal".to_string(), "tax".to_string()]),
            "a comma inside a label is part of the label"
        );
        assert_eq!(gate_of(Some(&["public"])), None);
        assert_eq!(gate_of(Some(&[" public "])), None);
        assert_eq!(gate_of(None), None);
    }
}
