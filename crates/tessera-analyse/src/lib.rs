//! The analysers for `text` columns, and the fold the value-suggestion index shares with them.
//!
//! An analyser is a named, versioned pipeline from a string to its tokens. A `text` column
//! declares one by name, and the manifest records the analyser's full identity against the column.
//! Analysers are built in. A loaded one would make the token stream depend on the deployment, and
//! the golden vectors in `tests/golden.rs` could then pin only a default.
//!
//! One analyser ships, [`UNICODE`]. Its stages are NFKC normalisation, full Unicode case folding,
//! then UAX #29 word segmentation. Normalising first gives the case folder and the segmenter one
//! spelling of each character (`ﬁ` and `fi`, halfwidth and fullwidth katakana, the Kelvin sign and
//! `K`), so a query and a document that differ only in spelling produce the same tokens.
//!
//! The segmenter is icu4x's [`WordSegmenter::new_auto`], which chooses its method per script run:
//! dictionary segmentation for Chinese, Japanese, Thai, Lao, Khmer and Burmese, and the plain
//! UAX #29 rules elsewhere. Those six scripts are written without spaces between words, so a
//! tokeniser that splits on non-alphanumerics returns a sentence as one token. One analyser serves
//! a mixed-script column with nothing declared.
//!
//! `unicode` has no stemming, stopwords, diacritic folding or synonyms. Each depends on the
//! language (`ö` and `o` are one letter in German and two in Swedish), and a wrong default loses
//! matches without an error. A pipeline that wants them is a new name with its own vectors.
//!
//! A token stream does not describe itself. An index built under one identity and queried under
//! another fails to match the strings whose segmentation differs, and two layers' postings can be
//! merged only when the same analyser produced both. Every reader and writer of a text index
//! therefore resolves its analyser with [`analyser_with_identity`] and refuses a column whose
//! recorded identity this binary cannot reproduce.
//!
//! The conformance oracle reaches this analyser through `tessera tokenise` and does not reimplement
//! it: PyICU wraps ICU4C, whose segmentation can differ from icu4x's, so a second implementation
//! would compare the two libraries. The independent check is the golden vectors, which are known
//! answers per script family.

use std::borrow::Cow;

use icu_casemap::{CaseMapper, CaseMapperBorrowed};
use icu_normalizer::{ComposingNormalizer, ComposingNormalizerBorrowed};
use icu_properties::props::{Alphabetic, GeneralCategory, GeneralCategoryGroup};
use icu_properties::{
    CodePointMapData, CodePointMapDataBorrowed, CodePointSetData, CodePointSetDataBorrowed,
};
use icu_segmenter::options::WordBreakInvariantOptions;
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed};

/// The name a `text` column declares to select the general prose pipeline.
pub const UNICODE: &str = "unicode";

/// `unicode`'s version. The `icu4x` part is the crate minor whose compiled data this binary
/// carries, and `Cargo.toml` pins the four icu4x crates to it exactly, so a data change is an edit
/// that has to move this constant. The `p` part is the pipeline's shape: it moves when a stage or
/// the token rule changes. Either change is a rebuild of every column indexed under the old value.
///
/// Every input to the token stream comes from that pin. The token rule reads icu4x's property
/// tables and not `char::is_alphanumeric`, whose tables move with the Rust toolchain.
const UNICODE_VERSION: &str = "icu4x-2.2/p1";

/// SHA-256 of each analyser's golden vectors, kept beside the version they were recorded under.
///
/// [`Analyser::identity`] does not depend on what the tokeniser does, so vectors regenerated from
/// changed behaviour would pass every token assertion with the version unmoved. Editing
/// `tests/vectors/golden.json` fails `tests/golden.rs` until this digest moves, and the line to
/// edit is next to the version that should move with it. `tests/golden.rs` holds the canonical
/// form and checks that every name in [`ANALYSER_NAMES`] has a row.
pub const ANALYSER_VECTOR_DIGESTS: &[(&str, &str)] = &[(
    UNICODE,
    "8f5b0d5efb3708f2f6d2d7a88ad56f163ffb91a03874908c38bd5bcda1eddbbd",
)];

/// Every analyser name this binary carries.
pub const ANALYSER_NAMES: &[&str] = &[UNICODE];

/// The analyser a column declares by `name`. `None` for a name this binary does not carry; there
/// is no default, because a fallback would index a column with a pipeline it did not declare.
pub fn analyser(name: &str) -> Option<Analyser> {
    match name {
        UNICODE => Some(Analyser::new()),
        _ => None,
    }
}

/// The analyser whose recorded identity is `identity`. `None` where this binary does not carry
/// that name, or carries it at another version: its terms could not be reproduced.
pub fn analyser_with_identity(identity: &str) -> Option<Analyser> {
    analyser(declared_name(identity)).filter(|a| a.identity() == identity)
}

/// The declared name inside an identity, which is `<name>/<version>`.
pub fn declared_name(identity: &str) -> &str {
    identity.split('/').next().unwrap_or_default()
}

/// The two character classes the token rule and the word-boundary rule read, from icu4x's tables
/// so that [`UNICODE_VERSION`] covers them.
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
/// Constructing one deserialises the segmenter's dictionaries, so build one per flush or build
/// stage and pass it down. It holds no request state and is `Send + Sync`. The icu4x handles are
/// not `Clone`; a holder that must clone shares it behind an `Arc`.
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

    /// `<name>/<version>`, recorded in the manifest against every column this analyser indexed.
    pub fn identity(&self) -> String {
        format!("{UNICODE}/{UNICODE_VERSION}")
    }

    /// The tokens of `text`, in order, with duplicates kept. `match` needs neither, but a phrase
    /// or scoring consumer needs both; the index deduplicates when it builds a posting.
    pub fn tokens(&self, text: &str) -> Vec<String> {
        let mut scratch = TokenScratch::default();
        let mut out = Vec::new();
        self.for_each_token(text, &mut scratch, &mut |token| out.push(token.to_string()));
        out
    }

    /// [`Self::tokens`], borrowed, over a buffer the caller keeps between documents. An index
    /// looks most tokens up and keeps few, so a sweep over a column allocates only for the terms
    /// it keeps.
    ///
    /// A segment is a token when it holds at least one alphanumeric character. The segmenter's own
    /// `is_word_like()` is not used: icu_segmenter 2.2 reports any run its dictionary resolves to a
    /// single word as not word-like, so `中文`, `日本語` and `ភាសាខ្មែរ` would yield no tokens and
    /// a document holding exactly `中文` could not be found by that query. Whitespace, punctuation
    /// and symbol runs hold no alphanumeric character and are dropped under either rule.
    ///
    /// Segmentation quality in the scripts without spaces is imperfect: Japanese `はとても` splits
    /// as `はと`/`て`/`も` and Thai `มาก` as `มา`/`ก`, so a query for the mis-split word misses.
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

/// The buffer [`Analyser::for_each_token`] reuses across documents. The caller holds it because an
/// [`Analyser`] is shared across threads.
#[derive(Default)]
pub struct TokenScratch {
    normalised: String,
}

/// NFKC, then full Unicode case folding: the analyser's first two stages, which the suggestion
/// index applies to its entries and queries too, so both surfaces fold by one rule.
struct Fold {
    nfkc: ComposingNormalizerBorrowed<'static>,
    case: CaseMapperBorrowed<'static>,
}

impl Fold {
    fn new() -> Self {
        Fold {
            nfkc: ComposingNormalizer::new_nfkc(),
            case: CaseMapper::new(),
        }
    }

    /// `text` folded, with the normalised copy written into a buffer the caller reuses. The case
    /// folder allocates its own output where the text is not already folded: writing it into a
    /// buffer needs the `writeable` crate as a direct dependency tracking icu4x's version.
    fn fold_into<'a>(&self, text: &str, normalised: &'a mut String) -> Cow<'a, str> {
        // `normalize` would borrow an already-normalised input, but finding that out costs a full
        // normalising pass, so the copy is written unconditionally.
        normalised.clear();
        let _ = self.nfkc.normalize_to(text, normalised);
        self.case.fold_string(&*normalised)
    }
}

/// The rules of a category vocabulary's suggestion index: which entry strings a value contributes
/// and where each begins in the string a response serves. They say nothing about how a `text`
/// column is tokenised.
///
/// An entry string is folded as the analyser folds, then has its whitespace collapsed and trimmed.
/// The same is applied to a typed query, so a match is a byte-prefix comparison.
///
/// A word starts at a transition into a letter, digit or combining mark from any other character,
/// read from the same pinned tables as the token rule. A script written without spaces therefore
/// yields no word starts: a run of Han, Khmer or Thai is one word, and such a vocabulary is
/// suggested on whole-key and whole-title prefixes alone. Dictionary segmentation would multiply
/// the index by the character count for those scripts and make the entry set depend on icu4x's
/// data version.
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

/// Which served string an entry came from, as `match.field` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SuggestionField {
    Key,
    Title,
}

impl SuggestionField {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SuggestionField::Key => "key",
            SuggestionField::Title => "title",
        }
    }
}

/// The three entry kinds, in the order ties between them break: a whole-key match, then a
/// whole-title match, then a word start inside a longer string.
///
/// The discriminants are the sort key and are written to the suggestion index, so reordering them
/// changes the order of a page.
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
    /// The folded, whitespace-collapsed, trimmed string a prefix is matched against.
    pub entry: String,
    pub kind: EntryKind,
    pub field: SuggestionField,
    /// The character offset into the served string (`key` or `title` as written) at which this
    /// entry begins. `match.start` reports it, and `match.len` is found by folding forward from it.
    pub start: u32,
}

impl SuggestionFold {
    pub fn new() -> Self {
        SuggestionFold {
            fold: Fold::new(),
            classes: Classes::new(),
        }
    }

    /// The entry string for `text`: folded, runs of whitespace collapsed to one space, trimmed.
    /// A string of whitespace alone gives the empty string.
    pub fn entry(&self, text: &str) -> String {
        self.entry_into(text, &mut String::new())
    }

    /// [`Self::entry`] with the normalisation buffer reused across strings.
    fn entry_into(&self, text: &str, normalised: &mut String) -> String {
        collapse_whitespace(&self.fold.fold_into(text, normalised))
    }

    /// Every entry one value contributes: the whole key, the whole title where there is one, and
    /// every word start after the first of the title, or of the key where there is no title.
    ///
    /// A whole entry that folds to the empty string is dropped: it would sit at the lower bound of
    /// every prefix range and stands for nothing a caller could type.
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

    /// The word-start entries of one served string and its entry string.
    ///
    /// The fold has no offset map: icu4x normalises and folds whole strings. The k-th word start
    /// of `served` is instead paired with the k-th of `entry`. The character before a word start
    /// is not a letter, digit or mark, and NFKC does not compose across such a character, so
    /// folding from a word start gives the tail of the whole string's fold.
    ///
    /// NFKC can move a boundary: `¼` becomes `1⁄4`, two words where there was one non-word
    /// character. Where the two lists differ in length the value gets no word-start entries.
    /// Equal lengths do not prove the pairing: `a㎏b ¼` folds to `akgb 1⁄4`, and the two changes
    /// cancel. A mis-paired `start` reaches only `match.start` and `match.len`, a highlight over
    /// the wrong characters. The entry string is the fold's own output, so which values match, and
    /// which are served, does not depend on it.
    fn word_entries(
        &self,
        served: &str,
        entry: &str,
        field: SuggestionField,
    ) -> Vec<SuggestionEntry> {
        let folded_starts = self.word_starts(entry.char_indices());
        let served_starts = self.word_starts(served.chars().enumerate());
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

    /// The position of every word start after the first, over `(position, character)` pairs:
    /// byte positions for an entry string, character positions for a served one.
    ///
    /// The first word is skipped wherever it begins, because the whole-string entry stands for it.
    fn word_starts(&self, chars: impl Iterator<Item = (usize, char)>) -> Vec<usize> {
        let mut previous = false;
        chars
            .filter_map(|(at, c)| {
                let wordish = self.wordish(c);
                let starts = wordish && !previous;
                previous = wordish;
                starts.then_some(at)
            })
            .skip(1)
            .collect()
    }

    /// The token rule's classes plus `Mark`. A combining mark is not `Alphabetic`, so without it
    /// the Thai `เป็น` would break at U+0E47, as would any Devanagari, Arabic or Hebrew word
    /// carrying a diacritic. A mark does not begin a word in well-formed text, so admitting it
    /// only joins runs.
    fn wordish(&self, c: char) -> bool {
        self.classes.alphanumeric(c) || self.classes.mark(c)
    }
}

/// The character index at which the whole-string entry begins in `served`: past the leading
/// whitespace the entry fold trims.
fn served_start(served: &str) -> usize {
    served
        .chars()
        .position(|c| !c.is_whitespace())
        .unwrap_or(0)
}

/// Runs of whitespace collapsed to one space, and the result trimmed.
///
/// `split_whitespace` reads std's `White_Space` table, which moves with the toolchain. No stored
/// identity depends on it: a code point becoming whitespace changes how one title collapses, and
/// the suggestion index is derived and rebuilt.
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
        s.word_starts(entry.char_indices())
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

    /// The first word is skipped wherever it begins, in the entry string and the served string
    /// alike. If the two disagreed, `entries_of` would drop every word start of the value.
    #[test]
    fn a_leading_non_word_character_does_not_make_the_first_word_a_start() {
        let s = SuggestionFold::new();
        for (served, want) in [
            ("(cs.LG)", vec!["lg)"]),
            ("[Draft] Machine Learning", vec!["machine learning", "learning"]),
            ("  ...cs.LG", vec!["lg"]),
        ] {
            let entries = s.entries_of("k", Some(served));
            let starts: Vec<&str> = entries
                .iter()
                .filter(|e| e.kind == EntryKind::WordStart)
                .map(|e| e.entry.as_str())
                .collect();
            assert_eq!(starts, want, "{served:?}");
        }
    }

    #[test]
    fn a_script_without_spaces_has_no_word_starts() {
        let s = SuggestionFold::new();
        for sample in ["中文分词测试", "日本語のテキスト", "ภาษาไทยเป็นภาษา"] {
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
                ("machine learning", EntryKind::Title, SuggestionField::Title, 0),
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

    /// `start` counts characters of the served string, so leading whitespace and a non-ASCII
    /// character both shift it away from the byte offset in the folded form.
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
        assert!(entries.iter().all(|e| e.kind != EntryKind::WordStart), "{entries:?}");
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

    /// The numeric half of the token rule. `٣٤٥` is `Nd` and not `Alphabetic`, and NFKC leaves it
    /// alone, so a rule that kept `Alphabetic` only would drop it.
    #[test]
    fn a_segment_of_digits_alone_is_a_token() {
        let a = Analyser::new();
        assert_eq!(a.tokens("2026"), vec!["2026"]);
        assert_eq!(a.tokens("٣٤٥"), vec!["٣٤٥"]);
        assert_eq!(a.tokens("accession 2026"), vec!["accession", "2026"]);
    }
}
