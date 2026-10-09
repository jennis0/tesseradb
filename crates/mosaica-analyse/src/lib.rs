//! Text analysis: the analysers that turn a `text` column's strings into tokens, and the
//! normalisation that the value-suggestion index shares with them.
//!
//! An analyser is a named, versioned pipeline from a string to its tokens. A `text` column
//! declares an analyser by name, and the manifest records the analyser's identity
//! (`<name>/<version>`) for that column. Analysers are compiled in. If one could be loaded at run
//! time, the tokens would depend on the deployment, and the golden vectors in `tests/golden.rs`
//! could check only the built-in one.
//!
//! There is one analyser, [`UNICODE`]. It applies NFKC normalisation, then full Unicode case
//! folding, then UAX #29 word segmentation. Normalisation runs first so that the later stages see
//! one spelling of each character (`ﬁ` and `fi`, halfwidth and fullwidth katakana, the Kelvin sign
//! and `K`). A query and a document that differ only in spelling then produce the same tokens.
//!
//! The segmenter is icu4x's [`WordSegmenter::new_auto`], which chooses its method per script run:
//! dictionary segmentation for Chinese, Japanese, Thai, Lao, Khmer and Burmese, and the plain
//! UAX #29 rules elsewhere. Those six scripts are written without spaces between words, so a
//! tokeniser that splits on non-alphanumeric characters returns a whole sentence as one token.
//! Because the method is chosen per run, a column that mixes scripts needs no configuration.
//!
//! `unicode` does no stemming, stopword removal, diacritic removal or synonym expansion. Each of
//! those depends on the language (`ö` and `o` are one letter in German and two in Swedish), and a
//! wrong choice loses matches without reporting an error. A pipeline that does any of them would
//! be a second analyser with its own name and golden vectors.
//!
//! Tokens do not record which analyser produced them. If an index is built with one analyser
//! version and queried with another, the strings that the two versions segment differently do not
//! match, and no error is reported. Two layers' postings can be merged only if the same version
//! produced both. Every reader and writer of a text index therefore gets its analyser from
//! [`analyser_with_identity`], which returns `None` unless this binary has exactly the recorded
//! version.
//!
//! The conformance oracle tokenises by running `tessera tokenise`. It has no tokeniser of its own,
//! because the Python binding (PyICU) wraps ICU4C, whose segmentation can differ from icu4x's, and
//! a comparison would then test one library against the other. The golden vectors are the
//! independent check: expected tokens for each script family, reviewed by a person.

use std::borrow::Cow;

use icu_casemap::{CaseMapper, CaseMapperBorrowed};
use icu_normalizer::{ComposingNormalizer, ComposingNormalizerBorrowed};
use icu_properties::props::{Alphabetic, GeneralCategory, GeneralCategoryGroup};
use icu_properties::{
    CodePointMapData, CodePointMapDataBorrowed, CodePointSetData, CodePointSetDataBorrowed,
};
use icu_segmenter::options::WordBreakInvariantOptions;
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed};

/// The name a `text` column declares to use the general-purpose analyser.
pub const UNICODE: &str = "unicode";

/// The version of `unicode`. The `icu4x` part is the minor version of the icu4x crates, whose
/// compiled Unicode data is built into this binary. `Cargo.toml` requires exactly that version, so
/// upgrading the data means editing `Cargo.toml`, and this constant must be changed in the same
/// edit. The `p` part numbers the pipeline itself: increase it when a stage or the token rule
/// changes. After either change, every column indexed under the old version must be rebuilt.
///
/// All Unicode data the tokens depend on comes from those crates. The token rule reads icu4x's
/// property tables, not `char::is_alphanumeric`, because std's tables change with the Rust
/// toolchain.
const UNICODE_VERSION: &str = "icu4x-2.2/p1";

/// The SHA-256 of each analyser's golden vectors, kept next to the analyser's version.
///
/// The version string is written by hand and does not change when the tokeniser's behaviour
/// changes. If the vectors were regenerated from changed behaviour, every token assertion would
/// pass with the version unchanged. This digest catches that case: any edit to
/// `tests/vectors/golden.json` fails `tests/golden.rs` until the digest is updated here, next to
/// the version that should change with it. `tests/golden.rs` defines the bytes that are hashed
/// and checks that every name in [`ANALYSER_NAMES`] has a digest.
pub const ANALYSER_VECTOR_DIGESTS: &[(&str, &str)] = &[(
    UNICODE,
    "8f5b0d5efb3708f2f6d2d7a88ad56f163ffb91a03874908c38bd5bcda1eddbbd",
)];

/// The name of every analyser in this binary.
pub const ANALYSER_NAMES: &[&str] = &[UNICODE];

/// The analyser called `name`, or `None` if this binary has no analyser of that name. An unknown
/// name is not replaced with a default, because the column would then be indexed by an analyser
/// it did not declare.
pub fn analyser(name: &str) -> Option<Analyser> {
    match name {
        UNICODE => Some(Analyser::new()),
        _ => None,
    }
}

/// The identity, `<name>/<version>`, that the manifest records for a column declaring `name`.
/// This does not construct the analyser, so checking a declaration does not load the segmenter's
/// dictionaries.
pub fn identity_of(name: &str) -> Option<String> {
    match name {
        UNICODE => Some(format!("{UNICODE}/{UNICODE_VERSION}")),
        _ => None,
    }
}

/// The analyser whose identity is `identity`, or `None` if this binary has no analyser of that
/// name or has a different version of it. A different version may tokenise differently, so it
/// must not read or extend the column's index.
pub fn analyser_with_identity(identity: &str) -> Option<Analyser> {
    let name = declared_name(identity);
    (identity_of(name)? == identity)
        .then(|| analyser(name))
        .flatten()
}

/// The analyser name in an identity of the form `<name>/<version>`.
pub fn declared_name(identity: &str) -> &str {
    identity.split('/').next().unwrap_or_default()
}

/// The Unicode property tables that the token rule and the word-start rule read. They come from
/// icu4x, so [`UNICODE_VERSION`] identifies their contents.
struct Classes {
    alphabetic: CodePointSetDataBorrowed<'static>,
    general_category: CodePointMapDataBorrowed<'static, GeneralCategory>,
}

impl Classes {
    fn new() -> Self {
        Classes {
            alphabetic: CodePointSetData::new::<Alphabetic>(),
            general_category: CodePointMapData::<GeneralCategory>::new(),
        }
    }

    /// `Alphabetic`, or `General_Category` in `{Nd, Nl, No}`: std's definition of
    /// `char::is_alphanumeric`.
    fn alphanumeric(&self, c: char) -> bool {
        self.alphabetic.contains(c)
            || GeneralCategoryGroup::Number.contains(self.general_category.get(c))
    }

    fn mark(&self, c: char) -> bool {
        GeneralCategoryGroup::Mark.contains(self.general_category.get(c))
    }
}

/// The `unicode` analyser.
///
/// Constructing an `Analyser` deserialises the segmenter's dictionaries, so construct one per
/// flush or build stage and pass a reference down. An `Analyser` has no mutable state and is
/// `Send + Sync`. It is not `Clone`, because the icu4x types it holds are not; share it in an
/// `Arc` where a clone is needed.
pub struct Analyser {
    fold: Fold,
    words: WordSegmenterBorrowed<'static>,
    classes: Classes,
}

impl std::fmt::Debug for Analyser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Analyser({})", self.identity())
    }
}

impl Default for Analyser {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyser {
    pub fn new() -> Self {
        Analyser {
            fold: Fold::new(),
            words: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            classes: Classes::new(),
        }
    }

    /// The name a column declares to use this analyser.
    pub fn name(&self) -> &'static str {
        UNICODE
    }

    /// The normalisation and case-folding stages, without segmentation.
    pub fn fold(&self) -> &Fold {
        &self.fold
    }

    /// `<name>/<version>`. The manifest records it for every column this analyser indexes.
    pub fn identity(&self) -> String {
        identity_of(UNICODE).expect("`unicode` is carried")
    }

    /// The tokens of `text`, in order of appearance, including repeats. A `match` query needs
    /// neither the order nor the repeats, but phrase search and scoring need both. The index
    /// removes repeats when it builds a posting list.
    pub fn tokens(&self, text: &str) -> Vec<String> {
        let mut scratch = TokenScratch::default();
        let mut out = Vec::new();
        self.for_each_token(text, &mut scratch, &mut |token| out.push(token.to_string()));
        out
    }

    /// Calls `f` with each token of `text`, in the order [`Self::tokens`] returns them. The tokens
    /// are borrowed from `scratch`, which the caller reuses from one document to the next. An
    /// indexer has already seen most of the tokens it meets, so it allocates only for new terms.
    ///
    /// A segment is a token if it contains at least one alphanumeric character. Segments of
    /// whitespace, punctuation or symbols contain none and are dropped.
    ///
    /// The segmenter's own `is_word_like()` flag is not used. In icu_segmenter 2.2 it is false for
    /// a run that the dictionary resolves to a single word, so `中文`, `日本語` and `ភាសាខ្មែរ` would
    /// produce no tokens, and a document containing only `中文` could not be found by searching
    /// for `中文`.
    ///
    /// The dictionary segmentation makes mistakes in the scripts written without spaces. Japanese
    /// `はとても` is split as `はと`/`て`/`も` and Thai `มาก` as `มา`/`ก`. A query for a word that was
    /// split wrongly does not find the document.
    pub fn for_each_token(&self, text: &str, scratch: &mut TokenScratch, f: &mut impl FnMut(&str)) {
        let folded = self.fold.fold_into(text, &mut scratch.normalised);
        let mut breaks = self.words.segment_str(&folded);
        let Some(mut start) = breaks.next() else {
            return;
        };
        for end in breaks {
            let segment = &folded[start..end];
            if segment.chars().any(|c| self.classes.alphanumeric(c)) {
                f(segment);
            }
            start = end;
        }
    }
}

/// The buffer that [`Analyser::for_each_token`] reuses from one document to the next. The caller
/// owns it because an [`Analyser`] is shared between threads.
#[derive(Default)]
pub struct TokenScratch {
    normalised: String,
}

/// NFKC normalisation followed by full Unicode case folding. These are the analyser's first two
/// stages. The suggestion index applies them to its entries and to typed queries, so text search
/// and suggestion normalise strings identically.
pub struct Fold {
    nfkc: ComposingNormalizerBorrowed<'static>,
    case: CaseMapperBorrowed<'static>,
}

impl Default for Fold {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Fold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fold({UNICODE}/{UNICODE_VERSION})")
    }
}

impl Fold {
    pub fn new() -> Self {
        Fold {
            nfkc: ComposingNormalizer::new_nfkc(),
            case: CaseMapper::new(),
        }
    }

    /// `text`, NFKC-normalised and then case-folded.
    pub fn fold(&self, text: &str) -> String {
        self.fold_into(text, &mut String::new()).into_owned()
    }

    /// [`Self::fold`], with the NFKC output written into `normalised`, a buffer the caller
    /// reuses. The case-folding stage still allocates when it changes the text. Writing its
    /// output into a buffer as well would need the `writeable` crate as a direct dependency, kept
    /// at the version icu4x uses.
    pub fn fold_into<'a>(&self, text: &str, normalised: &'a mut String) -> Cow<'a, str> {
        // `normalize` returns a borrow when the input is already normalised, but it finds that
        // out by normalising the whole input, so it saves no work. `normalize_to` always copies.
        normalised.clear();
        let _ = self.nfkc.normalize_to(text, normalised);
        self.case.fold_string(&*normalised)
    }
}

/// The rules for a category vocabulary's suggestion index: which entry strings each value adds to
/// the index, and where each entry begins in the key or title as the user wrote it. These rules
/// do not affect how a `text` column is tokenised.
///
/// An entry string is normalised and case-folded by [`Fold`], then runs of whitespace are
/// collapsed and the ends trimmed. A typed query is treated the same way, so matching is a
/// comparison of byte prefixes.
///
/// A word starts at a letter, digit or combining mark that follows any other kind of character.
/// The character properties come from the same icu4x tables as the token rule. Text in a script
/// written without spaces, such as Han, Khmer or Thai, is one run of letters and so has one word.
/// A vocabulary in such a script is suggested by the start of the whole key or title only.
/// Splitting these scripts with the dictionary segmenter would add about one entry per character
/// and would make the set of entries depend on the icu4x data version.
pub struct SuggestionFold {
    fold: Fold,
    classes: Classes,
}

impl Default for SuggestionFold {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SuggestionFold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SuggestionFold({UNICODE}/{UNICODE_VERSION})")
    }
}

/// Whether an entry came from a value's key or its title. `match.field` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SuggestionField {
    Key,
    Title,
}

impl SuggestionField {
    /// The name used in the response.
    pub fn as_str(self) -> &'static str {
        match self {
            SuggestionField::Key => "key",
            SuggestionField::Title => "title",
        }
    }
}

/// The kind of an entry. Entries with equal strings are ordered by kind: a whole key first, then
/// a whole title, then a word start inside a longer string.
///
/// The discriminants are written to the suggestion index and sorted on, so changing them changes
/// the order of results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EntryKind {
    Key = 0,
    Title = 1,
    WordStart = 2,
}

impl EntryKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(EntryKind::Key),
            1 => Some(EntryKind::Title),
            2 => Some(EntryKind::WordStart),
            _ => None,
        }
    }
}

/// One entry of the suggestion index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionEntry {
    /// The entry string, which a query is matched against by prefix.
    pub entry: String,
    pub kind: EntryKind,
    pub field: SuggestionField,
    /// The character offset at which this entry begins in the key or title as written.
    /// `match.start` reports it, and the engine computes `match.len` by folding the text from
    /// this offset.
    pub start: u32,
}

impl SuggestionFold {
    pub fn new() -> Self {
        SuggestionFold {
            fold: Fold::new(),
            classes: Classes::new(),
        }
    }

    /// The normalisation and case folding alone, without whitespace collapsing.
    pub fn fold(&self) -> &Fold {
        &self.fold
    }

    /// The entry string for `text`: normalised and case-folded, with each run of whitespace
    /// replaced by one space and the ends trimmed. Text that is all whitespace gives `""`.
    pub fn entry(&self, text: &str) -> String {
        self.entry_into(text, &mut String::new())
    }

    /// [`Self::entry`], reusing `normalised` as the NFKC buffer.
    fn entry_into(&self, text: &str, normalised: &mut String) -> String {
        collapse_whitespace(&self.fold.fold_into(text, normalised))
    }

    /// The entries for one value: its whole key, its whole title if it has one, and one entry for
    /// each word start in the title, or in the key if there is no title. A word that starts where
    /// the whole entry starts gets no entry of its own.
    ///
    /// A key or title whose entry string is empty gets no entry. An empty entry would match every
    /// query, and no query a user types corresponds to it.
    pub fn entries_of(&self, key: &str, title: Option<&str>) -> Vec<SuggestionEntry> {
        let mut scratch = String::new();
        let key_entry = self.entry_into(key, &mut scratch);
        let title_entry = title.map(|title| self.entry_into(title, &mut scratch));
        let words = match (title, &title_entry) {
            (Some(title), Some(entry)) => self.word_entries(title, entry, SuggestionField::Title),
            _ => self.word_entries(key, &key_entry, SuggestionField::Key),
        };

        let whole = |served: &str, entry: String, kind, field| {
            (!entry.is_empty()).then(|| SuggestionEntry {
                entry,
                kind,
                field,
                start: served_start(served) as u32,
            })
        };
        let mut out = Vec::with_capacity(2 + words.len());
        out.extend(whole(key, key_entry, EntryKind::Key, SuggestionField::Key));
        out.extend(title.zip(title_entry).and_then(|(title, entry)| {
            whole(title, entry, EntryKind::Title, SuggestionField::Title)
        }));
        out.extend(words);
        out
    }

    /// The word-start entries for one key or title. `served` is the text as written and `entry`
    /// is its entry string.
    ///
    /// Each entry needs a word's position in both strings, and icu4x does not report which input
    /// character produced which output byte. The k-th word start in `served` is therefore assumed
    /// to be the k-th word start in `entry`. The character before a word start is not a letter,
    /// digit or mark, and NFKC does not combine characters across it, so folding `served` from a
    /// word start gives the same bytes as `entry` from the matching word start.
    ///
    /// NFKC can add or remove word starts: `¼` becomes `1⁄4`, which has two. If the two strings
    /// have different numbers of word starts, the value gets no word-start entries. Equal numbers
    /// do not guarantee a correct pairing: `a㎏b ¼` becomes `akgb 1⁄4`, where one word start is
    /// lost and one gained. A wrong pairing gives a wrong `start`, which affects only
    /// `match.start` and `match.len`, so the client highlights the wrong characters. It does not
    /// affect which values match or which values a viewer is sent, because the entry string is
    /// taken from `entry` itself.
    fn word_entries(
        &self,
        served: &str,
        entry: &str,
        field: SuggestionField,
    ) -> Vec<SuggestionEntry> {
        let folded_starts = self.word_starts(entry.char_indices(), 0);
        let served_starts = self.word_starts(served.chars().enumerate(), served_start(served));
        if folded_starts.len() != served_starts.len() {
            return Vec::new();
        }
        folded_starts
            .into_iter()
            .zip(served_starts)
            .map(|(at, start)| SuggestionEntry {
                entry: entry[at..].to_string(),
                kind: EntryKind::WordStart,
                field,
                start: start as u32,
            })
            .collect()
    }

    /// The positions of the word starts in `chars`, which yields `(position, character)` pairs.
    /// The caller passes byte positions for an entry string and character positions for text as
    /// written.
    ///
    /// A word that starts at `whole`, the position where the whole-string entry starts, is
    /// omitted, because that entry already matches it. A first word that follows a bracket is
    /// kept, so the query `draft` finds `[Draft] Notes`.
    fn word_starts(&self, chars: impl Iterator<Item = (usize, char)>, whole: usize) -> Vec<usize> {
        let mut previous = false;
        chars
            .filter_map(|(at, c)| {
                let wordish = self.wordish(c);
                let starts = wordish && !previous;
                previous = wordish;
                (starts && at != whole).then_some(at)
            })
            .collect()
    }

    /// Whether `c` is part of a word: alphanumeric, or a combining mark. Combining marks are not
    /// `Alphabetic`. If they were excluded, the Thai word `เป็น` would be split at U+0E47, and so
    /// would any Devanagari, Arabic or Hebrew word with a diacritic. Well-formed text has no word
    /// that begins with a mark, so including marks joins parts of a word and adds no word starts.
    fn wordish(&self, c: char) -> bool {
        self.classes.alphanumeric(c) || self.classes.mark(c)
    }
}

/// The character offset of the first non-whitespace character in `served`, which is where its
/// whole-string entry begins.
fn served_start(served: &str) -> usize {
    served.chars().position(|c| !c.is_whitespace()).unwrap_or(0)
}

/// `folded` with each run of whitespace replaced by one space and the ends trimmed.
///
/// `split_whitespace` uses std's `White_Space` table, which can change with the Rust toolchain.
/// That is acceptable here because the suggestion index is rebuilt every time the engine opens a
/// bundle, so no stored data depends on the table.
fn collapse_whitespace(folded: &str) -> String {
    let mut out = String::with_capacity(folded.len());
    for word in folded.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_starts<'a>(s: &SuggestionFold, entry: &'a str) -> Vec<&'a str> {
        s.word_starts(entry.char_indices(), 0)
            .into_iter()
            .map(|at| &entry[at..])
            .collect()
    }

    fn rows(entries: &[SuggestionEntry]) -> Vec<(&str, EntryKind, SuggestionField, u32)> {
        entries
            .iter()
            .map(|e| (e.entry.as_str(), e.kind, e.field, e.start))
            .collect()
    }

    #[test]
    fn an_entry_is_folded_collapsed_and_trimmed() {
        let s = SuggestionFold::new();
        assert_eq!(s.entry("  Machine\t\n  LEARNING  "), "machine learning");
        assert_eq!(s.entry("cs.LG"), "cs.lg");
        assert_eq!(s.entry("ﬁLE"), "file");
        assert_eq!(s.entry("ＦＵＬＬ　width"), "full width");
        assert_eq!(s.entry("   "), "");
    }

    #[test]
    fn word_starts_are_transitions_into_a_letter_or_digit() {
        let s = SuggestionFold::new();
        assert_eq!(
            word_starts(&s, "machine_learning 2026.v2"),
            vec!["learning 2026.v2", "2026.v2", "v2"]
        );
    }

    /// The whole entry for `[Draft] Machine Learning` begins with `[`, so a query for `draft` can
    /// only match a word-start entry.
    #[test]
    fn a_first_word_behind_a_non_word_character_is_a_word_start() {
        let s = SuggestionFold::new();
        for (served, want) in [
            ("(cs.LG)", vec![("cs.lg)", 1), ("lg)", 4)]),
            (
                "[Draft] Machine Learning",
                vec![
                    ("draft] machine learning", 1),
                    ("machine learning", 8),
                    ("learning", 16),
                ],
            ),
            ("  ...cs.LG", vec![("cs.lg", 5), ("lg", 8)]),
            ("  cs.LG", vec![("lg", 5)]),
        ] {
            let entries = s.entries_of("k", Some(served));
            let starts: Vec<(&str, u32)> = entries
                .iter()
                .filter(|e| e.kind == EntryKind::WordStart)
                .map(|e| (e.entry.as_str(), e.start))
                .collect();
            assert_eq!(starts, want, "{served:?}");
        }
    }

    #[test]
    fn a_script_without_spaces_has_no_word_starts() {
        let s = SuggestionFold::new();
        for sample in ["中文分词测试", "日本語のテキスト", "ภาษาไทยเป็นภาษา"]
        {
            assert!(word_starts(&s, &s.entry(sample)).is_empty(), "{sample:?}");
        }
    }

    #[test]
    fn a_value_contributes_a_key_a_title_and_its_word_starts() {
        let s = SuggestionFold::new();
        assert_eq!(
            rows(&s.entries_of("cs.LG", Some("Machine Learning"))),
            vec![
                ("cs.lg", EntryKind::Key, SuggestionField::Key, 0),
                (
                    "machine learning",
                    EntryKind::Title,
                    SuggestionField::Title,
                    0
                ),
                ("learning", EntryKind::WordStart, SuggestionField::Title, 8),
            ]
        );
    }

    #[test]
    fn a_value_with_no_title_takes_its_word_starts_from_its_key() {
        let s = SuggestionFold::new();
        assert_eq!(
            rows(&s.entries_of("machine_learning", None)),
            vec![
                ("machine_learning", EntryKind::Key, SuggestionField::Key, 0),
                ("learning", EntryKind::WordStart, SuggestionField::Key, 8),
            ]
        );
    }

    #[test]
    fn a_whole_entry_that_folds_to_nothing_is_dropped() {
        let s = SuggestionFold::new();
        assert_eq!(
            rows(&s.entries_of("k", Some("   "))),
            vec![("k", EntryKind::Key, SuggestionField::Key, 0)]
        );
    }

    /// `start` counts characters in the text as written. Leading whitespace and a multi-byte
    /// character both make it differ from the byte offset in the entry string.
    #[test]
    fn start_is_a_character_offset_into_the_served_string() {
        let s = SuggestionFold::new();
        assert_eq!(
            rows(&s.entries_of("k", Some("  Café Noir")))[1..],
            [
                ("café noir", EntryKind::Title, SuggestionField::Title, 2),
                ("noir", EntryKind::WordStart, SuggestionField::Title, 7),
            ]
        );
    }

    /// `¼` folds to `1⁄4`, so the served and folded boundary counts differ. The value keeps its
    /// whole-title entry and gets no word starts.
    #[test]
    fn a_value_whose_fold_moves_a_boundary_indexes_no_word_starts() {
        let s = SuggestionFold::new();
        let entries = s.entries_of("k", Some("a¼b"));
        assert!(
            entries.iter().all(|e| e.kind != EntryKind::WordStart),
            "{entries:?}"
        );
        assert!(entries.iter().any(|e| e.kind == EntryKind::Title));
    }

    /// The ligature and the fullwidth letters are NFKC's; the Turkish dotted capital, the sharp s
    /// and the omega are the case folder's.
    #[test]
    fn normalisation_and_folding_collapse_spellings_that_must_match() {
        let a = Analyser::new();
        for (left, right) in [
            ("ﬁle", "file"),
            ("ＦＵＬＬ", "full"),
            ("İstanbul", "i\u{307}stanbul"),
            ("STRASSE", "straße"),
            ("Ω", "ω"),
        ] {
            assert_eq!(a.tokens(left), a.tokens(right), "{left:?} and {right:?}");
        }
    }

    #[test]
    fn a_segment_with_no_letter_or_digit_is_not_a_token() {
        let a = Analyser::new();
        assert_eq!(a.tokens("hello, world!"), vec!["hello", "world"]);
        assert_eq!(a.tokens("a—b"), vec!["a", "b"]);
        assert_eq!(a.tokens("  \t\n "), Vec::<String>::new());
        assert_eq!(a.tokens(""), Vec::<String>::new());
    }

    /// Digits are tokens. `٣٤٥` has `General_Category = Nd`, is not `Alphabetic`, and is unchanged
    /// by NFKC, so a token rule that tested `Alphabetic` alone would drop it.
    #[test]
    fn a_segment_of_digits_alone_is_a_token() {
        let a = Analyser::new();
        assert_eq!(a.tokens("2026"), vec!["2026"]);
        assert_eq!(a.tokens("٣٤٥"), vec!["٣٤٥"]);
        assert_eq!(a.tokens("accession 2026"), vec!["accession", "2026"]);
    }
}
