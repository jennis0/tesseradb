//! Vocabularies declared while the service runs: `PUT /control/vocabularies/{name}` and
//! `PATCH /control/vocabularies/{name}/values` (`ingest.md` §1.3; decision 0136).
//!
//! A declaration is the `[[vocabulary]]` block minus its acquisition keys, with the values that
//! fit the request inline; a page adds more. The executor resolves the declaration against the
//! served manifest, appends a `VocabularyDeclare` record, and publishes a generation whose
//! manifest carries the vocabulary — so from the acknowledgement a `declared` category column may
//! name it, and its values are the ones that column may carry.
//!
//! **Codes are the server's and never the caller's** (per-point-attributes §3.1). Nothing on
//! either route accepts a code: the executor draws each one at the width the declaration named
//! and records it in the same fsync as the value (`VocabularyMinter::mint`). A caller supplying
//! codes would be the minting authority for a space the server owns, and neither the scatter nor
//! the never-reuse rule would survive it.
//!
//! **A value's title upserts; its identity does not** (per-point-attributes §2.2, §5; decision
//! 0136's amendment). A value is addressed by its key. The key-to-code binding is immutable and a
//! code is never reused, so nothing a row already carries can change meaning here; the code is an
//! internal optimisation and does not enter the argument. A title supplied for a held key replaces
//! the held title, and the acknowledgement says how many titles were updated. Every identity field
//! — the key's binding, and the vocabulary's width, kind, visibility and `reserved` — is a `409`
//! on a difference, because each is baked into rows or into what ingest may say.
//!
//! **The rules are the build's, transcribed**, on `crate::attributes`' argument: a declaration
//! the build accepts and this route refuses, or the reverse, is a feature that works at one door
//! and not the other (decision 0091). `tessera_build::config`'s vocabulary compile is the other
//! statement of them.

use tessera_lifecycle::wal::{DeclaredVocabularyValue, VocabularyDeclaration};
use tessera_lifecycle::{DeclaredValue, ExecError, VocabularyRequest};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{Manifest, ManifestVocabulary, ManifestVocabularyValue};
use tessera_store::vocabulary::{Vocabularies, VocabularyMinter};

/// The vocabularies declared at a running service and not yet written into a `MANIFEST.json` by a
/// fold, in declaration order: the segments manifest's `vocabularies` list, held live. Written
/// only by the executor; read at every side-manifest publication.
///
/// **The declaration half is held here and the values are read from the minters**, because a
/// minter is where a value becomes durable — a page, and a discovered mint at a window close,
/// both land there. A second copy of the value set would be a second place for the two to
/// disagree about which code a key carries.
#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeVocabularies {
    declared: Vec<ManifestVocabulary>,
}

impl RuntimeVocabularies {
    pub(crate) fn seed(declared: Vec<ManifestVocabulary>) -> Self {
        RuntimeVocabularies { declared }
    }

    pub(crate) fn push(&mut self, vocabulary: ManifestVocabulary) {
        self.declared.push(vocabulary);
    }

    pub(crate) fn holds(&self, name: &str) -> bool {
        self.declared.iter().any(|v| v.name == name)
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.declared.iter().map(|v| v.name.clone()).collect()
    }

    /// Complete current state, for a publication: each declaration with its values as the live
    /// minters hold them. A name with no minter keeps the values it was last published with,
    /// which cannot arise while every declaration seeds one, and is the answer that cannot drop a
    /// binding if it ever does.
    pub(crate) fn snapshot(&self, vocabularies: &Vocabularies) -> Vec<ManifestVocabulary> {
        self.declared
            .iter()
            .map(|held| match vocabularies.get(&held.name) {
                Some(minter) => ManifestVocabulary {
                    values: values_with_titles(minter),
                    ..held.clone()
                },
                None => held.clone(),
            })
            .collect()
    }

    /// Drop the vocabularies a fold has just written into `MANIFEST.json`, named at the
    /// publication. A declaration made while the fold ran is not among them and stays.
    pub(crate) fn retire_folded(&mut self, folded: &[String]) {
        self.declared.retain(|v| !folded.contains(&v.name));
    }
}

/// A minter's bindings as a manifest value list, ascending by key and carrying each title — the
/// shape a publication writes and a reopen seeds from.
///
/// `tessera_store::vocabulary::values_of` drops titles, because the extension list it feeds
/// carries only what a *mint* produced. A runtime declaration's values are authored, so the title
/// travels with the binding or the name a client draws is lost at the next restart.
pub(crate) fn values_with_titles(minter: &VocabularyMinter) -> Vec<ManifestVocabularyValue> {
    let bindings: Vec<(String, u32)> = minter
        .bindings()
        .map(|(key, code)| (key.to_string(), code))
        .collect();
    bindings
        .into_iter()
        .map(|(key, code)| ManifestVocabularyValue {
            title: minter.title_of(&key).map(str::to_string),
            key,
            code,
        })
        .collect()
}

/// What resolving a declaration against the served manifest decided.
pub(crate) enum Resolution {
    /// A vocabulary of this name already carries exactly this identity: nothing to declare, and
    /// the request's values are applied as a page.
    Existing,
    /// A new vocabulary, compiled to the manifest entry it becomes, with no values yet.
    New(Box<ManifestVocabulary>),
}

/// Resolve a declaration against the served manifest: refuse it, recognise the vocabulary already
/// held, or compile the one it declares.
pub(crate) fn resolve(
    request: &VocabularyRequest,
    manifest: &Manifest,
) -> Result<Resolution, ExecError> {
    let refused = |detail: String| ExecError::VocabularyRefused { detail };
    let name = request.name.as_str();
    check_name(name).map_err(refused)?;
    let Some(width) = ScalarType::parse(&request.width).filter(is_category_width) else {
        return Err(refused(format!(
            "vocabulary '{name}': `width = \"{}\"` is not a code space. A category's code is \
             stored at `u8`, `u16` or `u32` (per-point-attributes §3.6), and the width is baked \
             into every row that carries a code",
            request.width
        )));
    };
    for &code in &request.reserved {
        if code == 0 {
            return Err(refused(format!(
                "vocabulary '{name}': `reserved` names code 0, the *absent* sentinel, which is \
                 never assigned to a value (per-point-attributes §3.6)"
            )));
        }
        if code > usable_max(width) {
            return Err(refused(format!(
                "vocabulary '{name}': `reserved` names code {code}, which a {} code space cannot \
                 hold",
                request.width
            )));
        }
    }
    let mut keys = std::collections::BTreeSet::new();
    for value in &request.values {
        check_value_key(name, &value.key).map_err(refused)?;
        if !keys.insert(value.key.as_str()) {
            return Err(refused(format!(
                "vocabulary '{name}': value '{}' is named twice in one request. A value is a key \
                 and its properties, so two rows for one key are two answers to which properties \
                 it has",
                value.key
            )));
        }
    }
    // **A closed set with no values is refused at both doors** (the build's own rule): the set is
    // the authority on what may be ingested, so an empty one refuses every value while its column
    // costs its width in every row.
    if request.kind == tessera_types::vocabulary::VocabularyKind::Declared
        && request.values.is_empty()
    {
        return Err(refused(format!(
            "vocabulary '{name}': `value_set = \"closed\"` with no values. A closed set is the \
             authority on what may be ingested, so an empty one refuses every value while its \
             column costs its width in every row. Declare the values, or write \
             `value_set = \"open\"` to have them minted as they arrive"
        )));
    }

    let compiled = ManifestVocabulary {
        name: request.name.clone(),
        kind: request.kind,
        visibility: request.visibility,
        width: width.arrow_type_name().to_string(),
        values: Vec::new(),
        reserved: {
            let mut reserved = request.reserved.clone();
            reserved.sort_unstable();
            reserved.dedup();
            reserved
        },
    };
    // A name the manifest holds is a held part (`ingest.md` §1.1): identical is accepted with no
    // effect and the request's values are applied as a page, different is a conflict. The values
    // are not part of the identity — they are the set part, and a set grows.
    if let Some(held) = manifest.vocabularies.iter().find(|v| v.name == name) {
        return if same_identity(held, &compiled) {
            Ok(Resolution::Existing)
        } else {
            Err(ExecError::VocabularyConflict {
                detail: format!(
                    "vocabulary '{name}' is already declared with a different value set, \
                     visibility, width or reserved list. A code's width is baked into every row \
                     that carries one, and a value set decides what ingest may say, so a name \
                     cannot change identity; declare the new vocabulary under another name"
                ),
            })
        };
    }
    Ok(Resolution::New(Box::new(compiled)))
}

/// Two declarations of one name, compared on everything but the value set.
///
/// **`reserved` is compared as a set.** It is a set of retired codes and nothing reads an order
/// into it, so a build that wrote `[7, 3]` and a request that writes `[3, 7]` declare one
/// vocabulary; comparing the two lists as written would answer `409` to a caller who resent their
/// own declaration.
fn same_identity(held: &ManifestVocabulary, compiled: &ManifestVocabulary) -> bool {
    held.kind == compiled.kind
        && held.visibility == compiled.visibility
        && held.width == compiled.width
        && normalised(&held.reserved) == normalised(&compiled.reserved)
}

/// A `reserved` list as a set: ascending, without repeats.
fn normalised(reserved: &[u32]) -> Vec<u32> {
    let mut out = reserved.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// Merge every live binding into the manifest's own vocabulary table, taking the live title where
/// there is one — what a fold writes into the next `MANIFEST.json` (`ingest.md` §1.3).
///
/// **A union, never a substitution.** The minters are seeded from every durable home a binding
/// lives in, so they are a superset of the table; taking them *instead* would still be a
/// derivation, and a derivation that missed a binding would recolour every row carrying its code.
/// Adding to what the table holds cannot do that. It is the fold's answer to a vocabulary
/// declared at a running service, whose served table entry carries the declaration and whose
/// values live in its minter until a publication reads them.
pub(crate) fn merge_live_values(manifest: &mut Manifest, vocabularies: &Vocabularies) {
    for vocabulary in &mut manifest.vocabularies {
        let Some(minter) = vocabularies.get(&vocabulary.name) else {
            continue;
        };
        for value in values_with_titles(minter) {
            match vocabulary
                .values
                .iter_mut()
                .find(|held| held.key == value.key)
            {
                // The key and the code are the table's own and do not move; the title is the
                // one thing a live minter may have changed (decision 0136's amendment). A minter
                // holding no title leaves the table's alone, a discovered value having none.
                Some(held) => {
                    if value.title.is_some() {
                        held.title = value.title;
                    }
                }
                None => vocabulary.values.push(value),
            }
        }
    }
}

/// The record the log carries for a declaration: the compiled identity, and the values the
/// request supplied with the codes the executor drew for them.
pub(crate) fn declaration_record(
    request: &VocabularyRequest,
    compiled: &ManifestVocabulary,
    codes: &[(String, u32)],
) -> VocabularyDeclaration {
    VocabularyDeclaration {
        name: compiled.name.clone(),
        title: request.title.clone(),
        kind: compiled.kind,
        visibility: compiled.visibility,
        width: compiled.width.clone(),
        values: codes
            .iter()
            .map(|(key, code)| DeclaredVocabularyValue {
                key: key.clone(),
                // **Recorded, never re-derived**: the code was drawn from OS entropy, which is
                // the one thing a replay cannot repeat (`WalRecord::VocabularyMint`).
                code: Some(*code),
                title: request
                    .values
                    .iter()
                    .find(|v| &v.key == key)
                    .and_then(|v| v.title.clone()),
            })
            .collect(),
        reserved: compiled.reserved.clone(),
    }
}

/// A replayed declaration as the manifest entry it becomes. The door validated the declaration
/// before the record was written, so what is compiled here is the record's own fields; a value
/// carrying no code is one no draw ever recorded, and is dropped rather than bound at a number
/// nobody assigned.
pub(crate) fn compile_record(declaration: &VocabularyDeclaration) -> ManifestVocabulary {
    ManifestVocabulary {
        name: declaration.name.clone(),
        kind: declaration.kind,
        visibility: declaration.visibility,
        width: declaration.width.clone(),
        values: declaration
            .values
            .iter()
            .filter_map(|v| {
                v.code.map(|code| ManifestVocabularyValue {
                    key: v.key.clone(),
                    code,
                    title: v.title.clone(),
                })
            })
            .collect(),
        reserved: declaration.reserved.clone(),
    }
}

/// Whether a stored type is a code space. `ScalarType::parse` reads every declarable width, and
/// three of them hold a category's code.
fn is_category_width(ty: &ScalarType) -> bool {
    matches!(ty, ScalarType::U8 | ScalarType::U16 | ScalarType::U32)
}

/// The highest code a width can hold — `tessera_store::vocabulary`'s own `usable_max`, which is
/// private to that module, restated for the `reserved` check.
fn usable_max(width: ScalarType) -> u32 {
    match width {
        ScalarType::U8 => u32::from(u8::MAX),
        ScalarType::U16 => u32::from(u16::MAX),
        _ => u32::MAX,
    }
}

/// A vocabulary's name is what an attribute's `vocabulary` key names and what `/v1/meta`
/// publishes, so it takes the column charset
/// (`tessera_build::config::check_column_name`, transcribed).
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a vocabulary with an empty name".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "vocabulary '{name}': a vocabulary's name is what an attribute's `vocabulary` key \
             names and what `/v1/meta` publishes, so it is limited to ASCII letters, digits, `_` \
             and `-`"
        ));
    }
    Ok(())
}

/// A value key is what a category column's rows carry on the wire (per-point-attributes §5), so
/// the spelling a defect produces is refused here.
fn check_value_key(vocabulary: &str, key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err(format!(
            "vocabulary '{vocabulary}': a value with an empty key. An empty string is what an \
             unset field and a client bug both produce, so minting for it would make a typo a \
             category (per-point-attributes §5)"
        ));
    }
    Ok(())
}

/// Check one page of values before any code is drawn, and count the titles it will change.
///
/// **A title upserts** (decision 0136's amendment): a value is addressed by its key, and a title
/// supplied for a held key replaces the held title. The count returned is how many held keys the
/// page gives a title they do not already carry, which is what the acknowledgement reports; a key
/// the page is about to bind is not among them, its title arriving with the value. Nothing here
/// refuses on a title, so the refusals left are the key's own spelling and a key named twice in
/// one page, which is two answers to what its title is.
pub(crate) fn check_page(
    minter: &VocabularyMinter,
    vocabulary: &str,
    values: &[DeclaredValue],
) -> Result<u64, ExecError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut titles = 0u64;
    for value in values {
        check_value_key(vocabulary, &value.key)
            .map_err(|detail| ExecError::VocabularyRefused { detail })?;
        if !seen.insert(value.key.as_str()) {
            return Err(ExecError::VocabularyRefused {
                detail: format!(
                    "vocabulary '{vocabulary}': value '{}' is named twice in one page. A value is \
                     a key and its properties, so two rows for one key are two answers to which \
                     properties it has",
                    value.key
                ),
            });
        }
        if let Some(supplied) = value.title.as_deref() {
            if minter.code_of(&value.key).is_some() && minter.title_of(&value.key) != Some(supplied)
            {
                titles += 1;
            }
        }
    }
    Ok(titles)
}
