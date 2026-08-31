//! The view roster: which views of which groups exist, and which keys are burnt
//! (`views.md` §3.2, §3.4; [decision 0108](../../../docs/decisions/0108-a-view-group-grows-by-its-roster.md)).
//!
//! Beside [`crate::registry`] and shaped like it, because the two problems are the same one: a
//! named object created while the service runs, made durable by a WAL record, carried forward for
//! ever by the segments manifest, and refused on recreation once dropped. What differs is what a
//! name costs — a layer's name carries entities and a view's carries a coordinate system — and
//! what each is measured against: a layer against its own declaration, a view against the group's.
//!
//! **What lives here is the runtime half.** The views a build declared are in `MANIFEST.json` and
//! are seeded in ([`ViewRoster::seed_declared`]) so that ordinals continue rather than restart;
//! what this structure *owns* is the creations and the tombstones, which is exactly what a
//! publication writes and a replay reads back.
//!
//! **Three rules the operations here exist to hold:**
//!
//! - **A roster record is immutable.** There is no update: an existing key is refused, and a wrong
//!   record is a drop and a recreate under a new key. The alternative is a narrowed gate that does
//!   not bite live sessions, a staleness the deny lane is not allowed and the roster is not either.
//! - **An ordinal is never reused, and neither is a key.** Both are burnt by a drop and the
//!   tombstone carries the ordinal, because a high-water recovered from the *live* views alone
//!   would reissue the newest one the moment it was the one dropped — and a reissued ordinal
//!   silently repoints every client cache keyed on the view (decision 0029).
//! - **Keys and ordinals belong to the group that owns them.** A group declaring `members` takes
//!   another group's (`views.md` §3.3), so a create names the owner and the sharing groups' copies
//!   are derived from that one record. Two records would be two places for them to disagree.

use std::collections::{BTreeMap, BTreeSet};

use tessera_types::view::{
    check_view_key, CreatedView, GroupMetadataField, TombstonedView, ViewMetadataType,
    ViewMetadataValue,
};

use crate::wal::WalRecord;

/// Why a create or a drop was refused. Every variant is the caller's own request measured against
/// state only the executor may read, which is why none of them is checkable in a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterError {
    /// The key is already a view of this group.
    Exists { group: String, key: String },
    /// The key was dropped, and a dropped key never comes back.
    Tombstoned { group: String, key: String },
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
            RosterError::Tombstoned { group, key } => write!(
                f,
                "view key '{group}:{key}' was dropped, and a dropped key is never reused \
                 (views §3.4). A recreated key with different contents would silently repoint \
                 every bookmark, every cached θ and every client cache keyed on the view \
                 (decision 0029) — a caller who wants the key again wants a different key"
            ),
            RosterError::Unknown { group, key } => write!(
                f,
                "no view '{group}:{key}' — nothing to drop"
            ),
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
    /// The group whose keys and ordinals these are, where this group declares `members`; `None`
    /// where it owns them.
    pub members_of: Option<&'a str>,
    /// The per-view metadata names and types this group declared.
    pub metadata: &'a [GroupMetadataField],
}

/// The roster a create and a drop operate on.
#[derive(Debug, Default, Clone)]
pub struct ViewRoster {
    /// The creations, in creation order — what a publication writes and a replay reads back.
    created: Vec<CreatedView>,
    /// Every key ever dropped, with the ordinal it burnt.
    tombstones: Vec<TombstonedView>,
    /// `(group, key)` of every view that exists right now, the build's included, so an existing
    /// key is one lookup rather than a walk of two lists and a subtraction.
    live: BTreeSet<(String, String)>,
    /// Per owning group, one past the highest ordinal ever issued — live, dropped or declared.
    next_ordinal: BTreeMap<String, u32>,
}

impl ViewRoster {
    pub fn new() -> Self {
        ViewRoster::default()
    }

    /// Seed the views a **build** declared: their keys are taken and their ordinals are spent.
    ///
    /// Called before [`Self::seed`] and before any WAL record, because ordinals are one sequence
    /// per group and the build's are its first members. A group's declared views are read off
    /// `MANIFEST.json`, which is the roster's other half and the half that never changes.
    pub fn seed_declared(&mut self, views: impl IntoIterator<Item = (String, String, u32)>) {
        for (group, key, ordinal) in views {
            self.live.insert((group.clone(), key));
            let next = self.next_ordinal.entry(group).or_insert(0);
            *next = (*next).max(ordinal + 1);
        }
    }

    /// Seed the runtime half from a segments manifest — the creations and the tombstones, exactly
    /// as they were published.
    ///
    /// **A tombstone is applied after the creation it retires**, whether or not that creation is
    /// still in the log: the two lists are complete current state, not a diff, so a key in both is
    /// a key that was created and then dropped.
    pub fn seed(&mut self, created: &[CreatedView], tombstones: &[TombstonedView]) {
        for view in created {
            self.admit(view.clone());
        }
        for stone in tombstones {
            self.retire(stone.clone());
        }
    }

    fn admit(&mut self, view: CreatedView) {
        let next = self.next_ordinal.entry(view.group.clone()).or_insert(0);
        *next = (*next).max(view.ordinal + 1);
        self.live.insert((view.group.clone(), view.key.clone()));
        if !self
            .created
            .iter()
            .any(|v| v.group == view.group && v.key == view.key)
        {
            self.created.push(view);
        }
    }

    fn retire(&mut self, stone: TombstonedView) {
        let next = self.next_ordinal.entry(stone.group.clone()).or_insert(0);
        *next = (*next).max(stone.ordinal + 1);
        self.live.remove(&(stone.group.clone(), stone.key.clone()));
        self.created
            .retain(|v| !(v.group == stone.group && v.key == stone.key));
        if !self
            .tombstones
            .iter()
            .any(|s| s.group == stone.group && s.key == stone.key)
        {
            self.tombstones.push(stone);
        }
    }

    /// The views created since the build, in creation order.
    pub fn created(&self) -> &[CreatedView] {
        &self.created
    }

    /// Every key ever dropped.
    pub fn tombstones(&self) -> &[TombstonedView] {
        &self.tombstones
    }

    /// Is this key a view of this group right now?
    pub fn is_live(&self, group: &str, key: &str) -> bool {
        self.live.contains(&(group.to_string(), key.to_string()))
    }

    pub fn is_tombstoned(&self, group: &str, key: &str) -> bool {
        self.tombstones
            .iter()
            .any(|s| s.group == group && s.key == key)
    }

    /// What a publication carries forward — complete current state, never a diff, on the posture
    /// the segments manifest already takes for `deny`, `tombstones` and `layers`.
    pub fn snapshot(&self) -> (Vec<CreatedView>, Vec<TombstonedView>) {
        (self.created.clone(), self.tombstones.clone())
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
        visibility: Option<String>,
        metadata: BTreeMap<String, ViewMetadataValue>,
    ) -> Result<WalRecord, RosterError> {
        if let Some(owner) = facts.members_of {
            return Err(RosterError::Refused(format!(
                "view group '{}' takes its views from group '{owner}' (views §3.3), so a key is \
                 created on '{owner}' and appears here at the same moment. Keys, ordinals and \
                 metadata belong to the group that owns them",
                facts.name
            )));
        }
        check_view_key(key).map_err(RosterError::Refused)?;
        if self.is_tombstoned(facts.name, key) {
            return Err(RosterError::Tombstoned {
                group: facts.name.to_string(),
                key: key.to_string(),
            });
        }
        if self.is_live(facts.name, key) {
            return Err(RosterError::Exists {
                group: facts.name.to_string(),
                key: key.to_string(),
            });
        }
        // **`public` or nothing** (`views.md` §6). No gate is evaluated anywhere and no
        // visible-view set exists, so accepting a label would register a view reachable by every
        // principal under a record saying otherwise — a disclosure control accepted and never
        // enforced, which is the one thing the fail-closed posture refuses outright. It is the
        // same refusal the declaration parser makes, for the same reason.
        if let Some(label) = &visibility {
            if label != "public" {
                return Err(RosterError::Refused(format!(
                    "visibility = '{label}'. A view's gate is specified and not implemented \
                     (views §6): no visible-view set is resolved at authorise and no view-valued \
                     surface is filtered, so a label here would be a control accepted and never \
                     enforced. `public` — the default, and what every view already is — is what \
                     this build can honour"
                )));
            }
        }
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
                ordinal: self.next_ordinal(facts.name),
                visibility,
                metadata,
            },
        })
    }

    /// The ordinal the next view of `group` takes — one past the highest ever issued there.
    fn next_ordinal(&self, group: &str) -> u32 {
        self.next_ordinal.get(group).copied().unwrap_or(0)
    }

    /// The record a `DELETE /control/views/{group}/{key}` appends, or the refusal.
    ///
    /// The ordinal travels in the record because it is burnt with the key: replay applies what was
    /// decided rather than re-deriving a number whose sequence has since moved.
    pub fn prepare_drop(
        &self,
        group: &str,
        key: &str,
        declared_ordinal: Option<u32>,
    ) -> Result<WalRecord, RosterError> {
        if !self.is_live(group, key) {
            // A tombstoned key and a key that never existed are the same answer, and deliberately:
            // the drop's own 404 is what a request naming the view gets from then on.
            return Err(RosterError::Unknown {
                group: group.to_string(),
                key: key.to_string(),
            });
        }
        let ordinal = self
            .created
            .iter()
            .find(|v| v.group == group && v.key == key)
            .map(|v| v.ordinal)
            .or(declared_ordinal)
            .ok_or_else(|| {
                RosterError::Refused(format!(
                    "view '{group}:{key}' holds no ordinal, so dropping it would burn nothing and \
                     the next create would reissue it"
                ))
            })?;
        Ok(WalRecord::ViewDrop {
            view: TombstonedView {
                group: group.to_string(),
                key: key.to_string(),
                ordinal,
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

    fn create(roster: &mut ViewRoster, group: &str, key: &str) -> Result<u32, RosterError> {
        let record = roster.prepare_create(facts(group, &[]), key, None, BTreeMap::new())?;
        let ordinal = match &record {
            WalRecord::ViewCreate { view } => view.ordinal,
            _ => unreachable!("prepare_create builds one variant"),
        };
        roster.apply(&record);
        Ok(ordinal)
    }

    #[test]
    fn ordinals_continue_past_the_build_and_past_a_drop() {
        let mut roster = ViewRoster::new();
        roster.seed_declared([("quarter".to_string(), "2026-Q1".to_string(), 0)]);
        assert_eq!(create(&mut roster, "quarter", "2026-Q2"), Ok(1));

        // Dropping the newest view must not hand its ordinal to the next create: the tombstone
        // carries it, which is why the high-water survives the removal.
        let drop = roster.prepare_drop("quarter", "2026-Q2", None).unwrap();
        roster.apply(&drop);
        assert_eq!(create(&mut roster, "quarter", "2026-Q3"), Ok(2));
    }

    #[test]
    fn a_key_is_refused_once_taken_and_for_ever_once_dropped() {
        let mut roster = ViewRoster::new();
        create(&mut roster, "quarter", "2026-Q2").unwrap();
        assert!(matches!(
            create(&mut roster, "quarter", "2026-Q2"),
            Err(RosterError::Exists { .. })
        ));
        let drop = roster.prepare_drop("quarter", "2026-Q2", None).unwrap();
        roster.apply(&drop);
        assert!(matches!(
            create(&mut roster, "quarter", "2026-Q2"),
            Err(RosterError::Tombstoned { .. })
        ));
        assert!(matches!(
            roster.prepare_drop("quarter", "2026-Q2", None),
            Err(RosterError::Unknown { .. })
        ));
    }

    #[test]
    fn a_drop_replays_without_its_own_create() {
        // The rotation case: the create's record is gone and the tombstone must still bite.
        let mut roster = ViewRoster::new();
        roster.seed_declared([("quarter".to_string(), "2026-Q1".to_string(), 0)]);
        roster.apply(&WalRecord::ViewDrop {
            view: TombstonedView {
                group: "quarter".to_string(),
                key: "2026-Q1".to_string(),
                ordinal: 0,
            },
        });
        assert!(!roster.is_live("quarter", "2026-Q1"));
        assert!(matches!(
            create(&mut roster, "quarter", "2026-Q1"),
            Err(RosterError::Tombstoned { .. })
        ));
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

    #[test]
    fn a_gate_that_is_not_public_is_refused_rather_than_recorded() {
        let roster = ViewRoster::new();
        assert!(roster
            .prepare_create(
                facts("quarter", &[]),
                "k",
                Some("finance".to_string()),
                BTreeMap::new()
            )
            .is_err());
        assert!(roster
            .prepare_create(
                facts("quarter", &[]),
                "k",
                Some("public".to_string()),
                BTreeMap::new()
            )
            .is_ok());
    }
}
