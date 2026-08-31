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
    Int,
    Float,
    Text,
    TimestampUs,
    /// A category: the value is a key resolved to its vocabulary's code (`views.md` §3.1).
    Category,
}

impl ViewMetadataType {
    pub fn name(self) -> &'static str {
        match self {
            ViewMetadataType::Bool => "bool",
            ViewMetadataType::Int => "integer",
            ViewMetadataType::Float => "float",
            ViewMetadataType::Text => "text",
            ViewMetadataType::TimestampUs => "timestamp_us",
            ViewMetadataType::Category => "category",
        }
    }

    /// Does `value` belong under this declaration?
    ///
    /// An integer is **not** accepted where a float is declared, and vice versa: the roster is
    /// served typed and a client reading `starts` as a float because one record happened to carry
    /// one is a client the declaration cannot help.
    pub fn admits(self, value: &ViewMetadataValue) -> bool {
        matches!(
            (self, value),
            (ViewMetadataType::Bool, ViewMetadataValue::Bool(_))
                | (ViewMetadataType::Int, ViewMetadataValue::Int(_))
                | (ViewMetadataType::Float, ViewMetadataValue::Float(_))
                | (ViewMetadataType::Text, ViewMetadataValue::Text(_))
                | (ViewMetadataType::TimestampUs, ViewMetadataValue::TimestampUs(_))
                | (ViewMetadataType::Category, ViewMetadataValue::Int(_))
        )
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
/// `views.md` §3.3), their copies are derived from this one record — the key and the ordinal
/// belong to the group that owns them, so a second record per sharing group would be a second
/// place for them to disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedView {
    pub group: String,
    pub key: String,
    /// Creation order within the group — monotone, **never reused**, an alias for the key.
    pub ordinal: u32,
    /// This view's own gate; `None` takes the group's.
    ///
    /// ⊘ **Recorded and never evaluated** (`views.md` §6): no gate is evaluated anywhere and no
    /// visible-view set exists, which is why the create refuses anything but `public`.
    pub visibility: Option<String>,
    pub metadata: BTreeMap<String, ViewMetadataValue>,
}

/// One dropped key (`views.md` §3.4).
///
/// **Carried for ever and never pruned**, exactly as a layer's tombstone is: a recreated key with
/// different contents would silently repoint every bookmark, every cached θ and every client cache
/// keyed on the view (decision 0029). The ordinal travels with it because it is burnt too — the
/// high-water is what the roster reads back, and an ordinal recovered from the live views alone
/// would be reissued the moment the newest view was the one dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TombstonedView {
    pub group: String,
    pub key: String,
    pub ordinal: u32,
}

/// A name a view id is built out of — a plain view's name, or a group's name or one of its keys
/// (`views.md` §3.2). `Err` is the refusal's own detail, so both callers say the same thing.
///
/// **The same charset a column name takes**, and for the same reason: a view id addresses a
/// directory in the bundle (`views/<group>/<key>/`), the `view` in a request body, the
/// `x-tessera-view` header and the manifest's `files` map, so it has to survive being a path
/// segment. Three characters are refused ahead of the charset because they are *reserved* rather
/// than merely outside it: `:` joins a group to its key, `#` marks an ordinal so a numeric-looking
/// key is not read as one, and `@` pins a group-scoped attribute to a view (`views.md` §5).
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
    for reserved in [':', '#', '@'] {
        if key.contains(reserved) {
            return Err(format!(
                "`{reserved}` is reserved out of a view name and a view key (views §3.2). A view \
                 of a group is addressed `<group>:<key>` or `<group>:#<ordinal>`, and a filter \
                 leaf pins a group-scoped attribute as `<column>@<key>` — so a name carrying one \
                 of `:`, `#` or `@` would make a request mean two things"
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
