//! The roster: what a view of a group carries, and what a create and a drop make durable
//! (`views.md` §3.2).
//!
//! Here rather than in `tessera-store` because three layers hold the same records and only one of
//! them is the manifest: the WAL record that makes a create durable (`tessera-lifecycle`), the
//! live roster the write path resolves against, and the segments manifest that carries the roster
//! forward for ever. The crate graph runs lifecycle → types and never lifecycle → store, so a
//! roster record defined beside the manifest could not travel in a log entry — the same argument
//! that put [`crate::layer::RegisteredLayer`] here.
//!
//! Behind the `serde` feature for [`crate::layer`]'s reason: these types exist to be written down.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What joins a group's name to one of its keys in a view id — `<group>:<key>` (`views.md` §3.2).
///
/// Here rather than beside the manifest's copy because the WAL's roster records travel through
/// `tessera-lifecycle`, which does not depend on `tessera-store`, and a second spelling of the
/// separator is how the two halves come to disagree about what a view id is.
pub const GROUP_SEPARATOR: char = ':';

/// Which incarnation of a key an artifact belongs to (decision 0115).
///
/// A counter rather than a random nonce: replay applies what was decided — the minted value
/// travels in the `ViewCreate` record — and a counter is the form a restart can also *check*, the
/// seed being one above every incarnation the manifests and the log carry, live or dead.
pub type ViewIncarnation = u64;

/// The incarnation of every view a **build** declared. A build coins each key once, so there is
/// nothing for it to distinguish; a key it declared that is later dropped and created again comes
/// back at 1 or above.
pub const DECLARED_INCARNATION: ViewIncarnation = 0;

/// One roster metadata value, typed against its group's declaration (`views.md` §3.1).
///
/// **View metadata is not an attribute** (`views.md` §5): one value per view rather than one per
/// `(entity, view)`, it filters nothing, and it is served typed. The two are kept apart so that
/// neither grows the other's surface.
/// **Externally tagged — `{"text": "Q1 2026"}` — and not `{type, value}`.** The same value is
/// written into a JSON manifest and into a postcard WAL record, and postcard is not
/// self-describing: serde's adjacently tagged representation cannot be deserialised from it, so a
/// record carrying one round-trips as a CRC-clean frame that will not decode — which the log reads
/// as corruption. The wire's `{type, value}` spelling is produced where the wire is produced
/// (`tessera-server`'s `viewer`), which is where a presentation shape belongs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMetadataValue {
    Bool(bool),
    /// Every integer width, and a category's key resolved to its code.
    Int(i64),
    Float(f64),
    Text(String),
    /// Microseconds since the Unix epoch — the one time unit a `timestamp_us` may hold, so a
    /// declaration and a reader cannot disagree about it.
    TimestampUs(i64),
}

impl ViewMetadataValue {
    /// The type name a refusal names, so a create that supplied the wrong shape is told which one
    /// the group declared rather than being told it was wrong.
    pub fn type_name(&self) -> &'static str {
        match self {
            ViewMetadataValue::Bool(_) => "bool",
            ViewMetadataValue::Int(_) => "integer",
            ViewMetadataValue::Float(_) => "float",
            ViewMetadataValue::Text(_) => "text",
            ViewMetadataValue::TimestampUs(_) => "timestamp_us",
        }
    }
}

/// The kind one declared metadata name takes.
///
/// **The declaration's own shape, not the value's.** A create supplies values and the roster
/// stores them; this is what says a supplied value belongs where it was put, and it is published
/// in the manifest because a group whose views are created while the service runs has no other
/// record of what its views must carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMetadataType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    Float,
    Text,
    TimestampUs,
    /// A category: the value is a key resolved to its vocabulary's code.
    Category,
}

impl ViewMetadataType {
    pub fn name(self) -> &'static str {
        match self {
            ViewMetadataType::Bool => "bool",
            ViewMetadataType::U8 => "u8",
            ViewMetadataType::U16 => "u16",
            ViewMetadataType::U32 => "u32",
            ViewMetadataType::U64 => "u64",
            ViewMetadataType::I8 => "i8",
            ViewMetadataType::I16 => "i16",
            ViewMetadataType::I32 => "i32",
            ViewMetadataType::I64 => "i64",
            ViewMetadataType::Float => "float",
            ViewMetadataType::Text => "text",
            ViewMetadataType::TimestampUs => "timestamp_us",
            ViewMetadataType::Category => "category",
        }
    }

    /// The values an integer type holds, or `None` for a type that is not an integer. A value is
    /// carried as an `i64`, so `u64` stops at `i64::MAX`.
    pub fn integer_range(self) -> Option<(i64, i64)> {
        Some(match self {
            ViewMetadataType::U8 => (0, i64::from(u8::MAX)),
            ViewMetadataType::U16 => (0, i64::from(u16::MAX)),
            ViewMetadataType::U32 => (0, i64::from(u32::MAX)),
            ViewMetadataType::U64 => (0, i64::MAX),
            ViewMetadataType::I8 => (i64::from(i8::MIN), i64::from(i8::MAX)),
            ViewMetadataType::I16 => (i64::from(i16::MIN), i64::from(i16::MAX)),
            ViewMetadataType::I32 => (i64::from(i32::MIN), i64::from(i32::MAX)),
            ViewMetadataType::I64 => (i64::MIN, i64::MAX),
            _ => return None,
        })
    }

    /// Does `value` belong under this declaration? An integer must fit the declared width. An
    /// integer is not accepted where a float is declared, because the roster is served typed.
    pub fn admits(self, value: &ViewMetadataValue) -> bool {
        match (self, value) {
            (ViewMetadataType::Bool, ViewMetadataValue::Bool(_))
            | (ViewMetadataType::Float, ViewMetadataValue::Float(_))
            | (ViewMetadataType::Text, ViewMetadataValue::Text(_))
            | (ViewMetadataType::TimestampUs, ViewMetadataValue::TimestampUs(_))
            // A category is stored as its code, which is how a build writes one.
            | (ViewMetadataType::Category, ViewMetadataValue::Int(_)) => true,
            (ty, ViewMetadataValue::Int(v)) => ty
                .integer_range()
                .is_some_and(|(min, max)| (min..=max).contains(v)),
            _ => false,
        }
    }
}

/// One declared per-view metadata name and its type, as the manifest publishes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMetadataField {
    pub name: String,
    pub ty: ViewMetadataType,
    /// The vocabulary a category's keys are drawn from; `None` for every other type.
    pub vocabulary: Option<String>,
}

/// One view created while the service runs — the roster record `PUT /control/views/{group}/{key}`
/// carries, made durable first by the WAL and then, for ever, by the segments manifest
/// (`views.md` §3.2, decision 0108).
///
/// **The record is immutable.** A wrong gate or wrong metadata is a drop and a recreate under a
/// new key, never an update: the alternative is a narrowed gate that does not bite live sessions.
///
/// **It names the owner group only.** Where other groups share these views (`members`,
/// `views.md` §3.3), their copies are derived from this one record — the key belongs to the group
/// that owns it, so a second record per sharing group would be a second place for them to
/// disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedView {
    pub group: String,
    pub key: String,
    /// Which **incarnation** of the key this record is
    /// ([decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)).
    ///
    /// **Internal, and on no wire.** A client addresses a view by `<group>:<key>` and by nothing
    /// else; this number never appears in `/v1/meta`, in a response, or in a view id. It exists
    /// because a dropped view's row spaces, columns and side-manifest entries outlive the drop
    /// until the fold reclaims them, and a key created again must not adopt them: every artifact
    /// of a view carries the incarnation it was written under, and only the live one is composed.
    ///
    /// **Minted monotonically and recorded, never re-derived** — the rule `VocabularyMint` and
    /// `LayerCreate` already follow. A build-declared view is incarnation 0, so a key first used
    /// at a build and dropped comes back at 1 or above.
    pub incarnation: ViewIncarnation,
    /// This view's own gate: the labels a principal must hold one of, each one term verbatim
    /// (`views.md` §6, decision 0132); `None` takes the group's. Never empty: a gate naming no
    /// terms is refused before a record is prepared.
    pub visibility: Option<Vec<String>>,
    pub metadata: BTreeMap<String, ViewMetadataValue>,
}

/// A view gate's `visibility` as a declaration or a request body spells it (`views.md` §6,
/// decision 0132): one label, or a list of labels. Each element is one label, taken verbatim; a
/// comma inside a label is part of the label. One label is the common case, and the list is how a
/// gate names several terms.
///
/// Untagged, so a TOML key or a JSON field accepts `"finance"` and `["finance", "legal"]` alike.
/// This is the wire and file shape only; the stored form is the list ([`CreatedView::visibility`]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum DeclaredGate {
    One(String),
    Many(Vec<String>),
}

impl DeclaredGate {
    /// The labels, one per element. A single label becomes a one-element list.
    pub fn into_labels(self) -> Vec<String> {
        match self {
            DeclaredGate::One(label) => vec![label],
            DeclaredGate::Many(labels) => labels,
        }
    }
}

/// One **dead incarnation** of a key — what a drop leaves behind (`views.md` §3.4,
/// [decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)).
///
/// **Not a refusal.** This list used to be a tombstone list and a create measured itself against
/// it; a dropped key is now reusable, and what survives is the bookkeeping the reuse needs. An
/// entry says *this incarnation of this key is dead, and its artifacts are unreachable until the
/// fold reclaims them* — which is what keeps a recreated key from adopting its predecessor's row
/// spaces, columns and derived structures, and what a reclaim reads to know what it may delete.
///
/// **Carried until the reclaim, not for ever in principle** — though nothing prunes it today: the
/// fold reclaims a dead incarnation's files by omission (`views.md` §3.4), and the entry is what
/// records that there was something to omit. The field is named for what it holds rather than
/// kept under the tombstone name it no longer earns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeadIncarnation {
    pub group: String,
    pub key: String,
    /// The incarnation that died. Every artifact stamped with it is unreachable from this moment.
    pub incarnation: ViewIncarnation,
}

/// A name a view id is built out of — a plain view's name, or a group's name or one of its keys
/// (`views.md` §3.2). `Err` is the refusal's own detail, so both callers say the same thing.
///
/// **The same charset a column name takes**, and for the same reason: a view id addresses a
/// directory in the bundle (`views/<group>/<key>/`), the `view` in a request body, the
/// `x-tessera-view` header and the manifest's `files` map, so it has to survive being a path
/// segment. Two characters are refused ahead of the charset because they are *reserved* rather
/// than merely outside it: `:` joins a group to its key, and `@` pins a group-scoped attribute to
/// a view (`views.md` §5).
///
/// **One definition, two entry points.** The declaration parser and the create operation are the
/// two places a key is coined (decision 0091: a build is ingest into an empty database), and a
/// second copy of the charset is how a key that builds and a key that creates come to differ.
pub fn check_view_key(key: &str) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("the name is empty. A view's name is its identity — it addresses the view on \
                    every verb and names its directory in the bundle"
            .to_string());
    }
    for reserved in [':', '@'] {
        if key.contains(reserved) {
            return Err(format!(
                "`{reserved}` is reserved out of a view name and a view key (views §3.2). A view \
                 of a group is addressed `<group>:<key>`, and a filter leaf pins a group-scoped \
                 attribute as `<column>@<key>` — so a name carrying `:` or `@` would make a \
                 request mean two things"
            ));
        }
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("a view name is limited to the column-name charset — ASCII letters, digits, \
                    `_` and `-` (views §3.2). It is a path segment in the bundle \
                    (`views/<group>/<key>/`) and an identifier on the wire, so the set is closed \
                    here rather than escaped at every use site"
            .to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integer_must_fit_the_declared_width() {
        assert!(ViewMetadataType::U8.admits(&ViewMetadataValue::Int(255)));
        assert!(!ViewMetadataType::U8.admits(&ViewMetadataValue::Int(256)));
        assert!(!ViewMetadataType::U8.admits(&ViewMetadataValue::Int(-1)));
        assert!(ViewMetadataType::I8.admits(&ViewMetadataValue::Int(-128)));
        assert!(!ViewMetadataType::Float.admits(&ViewMetadataValue::Int(1)));
        assert!(!ViewMetadataType::I32.admits(&ViewMetadataValue::Text("1".to_string())));
    }
}
