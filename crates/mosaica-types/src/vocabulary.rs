//! A vocabulary's declared properties, in the one place both of its durable homes can read.
//!
//! `MANIFEST.json` carries a built vocabulary and the WAL's `VocabularyDeclare` record carries one
//! declared while the service runs (`ingest.md` §1.3). The crate graph runs lifecycle → types and
//! never lifecycle → store, so the two discriminants a record and a manifest must agree on live
//! here, as the roster's do in [`crate::view`].

use serde::{Deserialize, Serialize};

/// Whether a vocabulary's value set is closed at build or grows as the corpus supplies keys
/// (per-point-attributes §3.4).
///
/// The distinction is only ever consulted at a write: it decides what happens to a key nothing
/// has bound yet. Every read path treats the two identically, because a bound key is a bound key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VocabularyKind {
    /// The value set is closed: every key is authored, and an unknown one is refused
    /// (declare-then-use, §5). A typo must not create a category.
    Declared,
    /// The value set grows: a key nothing has bound acquires a scattered code at the commit-window
    /// close, recorded beside it and pinned for ever (§3.4).
    Discovered,
}

/// Whether the existence of a value is sensitive (per-point-attributes §3.8), orthogonal to
/// [`VocabularyKind`]'s operational question.
///
/// One spelling from the declaration through to the wire (`configuration.md` §1): the config
/// word, the manifest discriminant and what `/v1/meta` publishes are the same two strings.
///
/// Typed rather than a string because it decides whether `/v1/categories` filters a value set per
/// principal, so a spelling no reader recognises must refuse the manifest at the parse rather than
/// fall through to a default. Both defaults are wrong in a direction that matters: `public`
/// publishes a gated value set, `derived` withholds a published one and looks like a permission
/// bug.
///
/// `Derived` is the membership axis and `Public` the label one (decision 0088): `Derived` says the
/// viewer must already see some member, where `Public` names an access label. They share a key
/// because that is what the configuration surface declares (`configuration.md` §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// The value set is filtered per principal: a value appears only if the principal can see at
    /// least one item carrying it (per-point-attributes §3.3).
    Derived,
    /// The value set is published as authored, to every principal with a session. Legal only for a
    /// `declared` vocabulary, where an accountable party wrote the names down (§3.8).
    Public,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Derived => "derived",
            Visibility::Public => "public",
        }
    }
}
