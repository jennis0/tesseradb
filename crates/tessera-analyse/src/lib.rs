//! The text family's analyser: one pipeline, language-agnostic, with no per-column configuration
//! (`records-and-search.md` §4.4).
//!
//! **Three stages, in this order, and the order is load-bearing.** NFKC normalisation, then full
//! Unicode case folding, then UAX #29 word segmentation. Normalising first means the case folder
//! and the segmenter see one spelling of each character rather than two — `ﬁ` and `fi`, the
//! halfwidth katakana and the fullwidth, the Kelvin sign and `K` — so a query and a document that
//! differ only in spelling produce the same tokens. Folding before segmenting matters for the
//! scripts where case affects a word boundary decision, and costs nothing for the rest.
//!
//! # Why icu4x rather than a tokeniser
//!
//! **Supporting a range of languages is a requirement of this design, not an aspiration**, and
//! that requirement is what rules out the obvious implementation. Split-on-non-alphanumerics is a
//! Latin-only tokeniser: Chinese, Japanese, Thai, Lao, Khmer and Burmese write without inter-word
//! spaces, so it returns whole sentences as single tokens and every `match` over them silently
//! fails. UAX #29 with dictionary-backed segmentation is the specified answer, and icu4x is
//! Unicode's own implementation of it.
//!
//! [`WordSegmenter::new_auto`] chooses its method **per script run**, so one analyser serves a
//! mixed-script corpus with nothing declared — an English abstract containing a Japanese title
//! segments both halves correctly in one pass.
//!
//! Stemming, stopwords, diacritic folding and synonyms are **deliberately absent** (§9). Each is
//! language-dependent — `ö` and `o` are the same letter in German and different letters in Swedish
//! — each is a conformance surface, and a wrong default corrupts recall silently rather than
//! loudly. If language-specific analysis is ever wanted it is a per-column `language` declaration
//! and a rebuild; the index format does not change, only the token stream.
//!
//! # The version is part of the artefact
//!
//! [`ANALYSER_VERSION`] is recorded in the manifest, and **changing it is a rebuild** — exactly as
//! changing a category's width is. A token stream is not self-describing: an index built under one
//! Unicode release and queried under another would fail to match on precisely the strings whose
//! segmentation changed, which is a silent recall bug rather than an error. The fold's merge
//! argument depends on it too (§7): two layers' postings may be merged only because they were
//! produced by the same versioned analyser over the same values.
//!
//! # One implementation, two accesses
//!
//! The conformance oracle derives its expected `match` results from the fixture's own values —
//! the fixture-input relation §3 records — and passes them through **this** analyser, reached by
//! the `tessera tokenise` verb rather than reimplemented. PyICU wraps ICU4C, whose segmentation can
//! diverge from icu4x's, so a second implementation would test the two libraries against each other
//! rather than testing Tessera. Independence lives instead in the **golden vectors**
//! (`tests/golden.rs`), which are known answers per script family, checked in, and pinned to the
//! version above.

use icu_casemap::{CaseMapper, CaseMapperBorrowed};
use icu_normalizer::{ComposingNormalizer, ComposingNormalizerBorrowed};
use icu_segmenter::options::WordBreakInvariantOptions;
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed};

/// The analyser's identity, recorded in the manifest and checked at open.
///
/// **It names the pipeline and the data, because either changing changes the token stream.** The
/// `icu4x` component is the crate major-minor whose compiled data this binary carries; the `p`
/// component is this pipeline's own shape, and it moves if a stage is added, removed or reordered
/// even when icu4x does not move.
///
/// A bundle carrying a different value is not readable by this binary: §7's merge argument and
/// every `match` answer depend on one token stream, and there is no way to detect the mismatch from
/// the postings themselves — two analysers disagree by producing *different but individually valid*
/// terms. Pre-release this is a fail-closed refusal rather than a migration (decision 0048).
pub const ANALYSER_VERSION: &str = "icu4x-2.2/p1";

/// The three stages, constructed once and reused.
///
/// **Construction is not free and tokenising is**, which is why this is a type rather than a
/// function: the segmenter's dictionary data is deserialised at construction, and a flush that
/// built one per row would pay that per row. Build one per flush (or per build stage) and pass it
/// down. It holds no request state and is `Send + Sync`.
pub struct Analyser {
    // The compiled-data constructors hand back `'static` borrows of data baked into the binary, so
    // these are handles rather than owned tables — the cost this type exists to amortise is the
    // segmenter's deserialisation, not an allocation.
    nfkc: ComposingNormalizerBorrowed<'static>,
    case: CaseMapperBorrowed<'static>,
    words: WordSegmenterBorrowed<'static>,
}

impl Default for Analyser {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyser {
    pub fn new() -> Self {
        Analyser {
            nfkc: ComposingNormalizer::new_nfkc(),
            case: CaseMapper::new(),
            // `new_auto` picks per script run: dictionary segmentation for Chinese, Japanese, Thai,
            // Lao, Khmer and Burmese, and the plain UAX #29 rules elsewhere. That per-run choice is
            // what lets one analyser serve a mixed-script corpus with nothing declared.
            words: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
        }
    }

    /// The tokens of `text`, in order, with duplicates kept.
    ///
    /// **Order and duplicates are preserved even though `match` needs neither**, because the
    /// positional payload sidecar (§4.5) and any future phrase or scoring consumer needs both, and
    /// an analyser that deduplicated here would make them re-analyse. The index deduplicates when
    /// it builds a posting; that is the index's business, not the analyser's.
    ///
    /// **A segment is a token when it contains at least one alphanumeric character**, which is not
    /// the same rule as the segmenter's own `is_word_like()` and deliberately so. Measured against
    /// icu_segmenter 2.2's compiled data, that flag drops content:
    ///
    /// **any run the dictionary resolves to a single word is reported not-word-like**, so `中文`,
    /// `日本語`, `x 日本語` and `ភាសាខ្មែរ` yield *no* tokens at all, while `中文分词测试`,
    /// `日本語のテキスト` and `ភាសាខ្មែរពិរោះណាស់` — the same scripts, segmented into two or more
    /// words — yield theirs.
    ///
    /// Either would be a silent recall failure of the worst kind: a document containing exactly
    /// `中文` would be unfindable by the query `中文`, with no error anywhere. Keeping a segment on
    /// its characters instead is strictly more conservative — whitespace, punctuation and symbol
    /// runs still carry no alphanumeric character and are still dropped — and it makes the failure
    /// mode *under-segmentation* (one token where two were wanted) rather than *no token at all*.
    ///
    /// **All six of §4.4's dictionary scripts segment**, Khmer included — surveyed across
    /// twenty-one scripts, every space-separated one returns exactly its source word count and
    /// every no-space one splits. ⊘ What is imperfect is segmentation *quality* in the no-space
    /// scripts: Japanese `はとても` splits as `はと`/`て`/`も` and Thai `มาก` as `มา`/`ก`, so a
    /// query for the mis-split word does not find the document. That is the gap `lindera` is the
    /// design's named escalation for, and it is a recall shortfall on word-internal queries rather
    /// than a coverage hole.
    pub fn tokens(&self, text: &str) -> Vec<String> {
        let normalised = self.nfkc.normalize(text);
        let folded = self.case.fold_string(&normalised);
        let mut out = Vec::new();
        let mut breaks = self.words.segment_str(&folded);
        let mut start = match breaks.next() {
            Some(first) => first,
            None => return out,
        };
        for end in breaks {
            let segment = &folded[start..end];
            if segment.chars().any(char::is_alphanumeric) {
                out.push(segment.to_string());
            }
            start = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipeline's order is what makes these equal, and each pair differs at a different stage:
    /// the ligature and the fullwidth digits are NFKC's, the Turkish dotted capital and the German
    /// sharp s are the case folder's.
    #[test]
    fn normalisation_and_folding_collapse_spellings_that_must_match() {
        let a = Analyser::new();
        for (left, right) in [
            ("ﬁle", "file"),
            ("ＦＵＬＬ", "full"),
            ("İstanbul", "i\u{307}stanbul"),
            ("STRASSE", "strasse"),
            ("Ω", "ω"),
        ] {
            assert_eq!(
                a.tokens(left),
                a.tokens(right),
                "{left:?} and {right:?} must analyse alike"
            );
        }
    }

    /// Punctuation, whitespace and symbols are not tokens — UAX #29's own word-like distinction,
    /// not a rule invented here.
    #[test]
    fn only_word_like_segments_survive() {
        let a = Analyser::new();
        assert_eq!(a.tokens("hello, world!"), vec!["hello", "world"]);
        assert_eq!(a.tokens("  \t\n "), Vec::<String>::new());
        assert_eq!(a.tokens(""), Vec::<String>::new());
        assert_eq!(a.tokens("a—b"), vec!["a", "b"]);
    }

    /// **Duplicates and order are kept.** `match` needs neither, but the positional payload the
    /// phrase and scoring upgrades share needs both, and an analyser that deduplicated here would
    /// force them to re-analyse.
    #[test]
    fn duplicates_and_order_survive() {
        let a = Analyser::new();
        assert_eq!(a.tokens("the cat the hat"), vec!["the", "cat", "the", "hat"]);
    }

    /// A script with no inter-word spaces segments into words rather than into one token — the
    /// property that rules out split-on-non-alphanumerics, asserted rather than assumed.
    #[test]
    fn a_script_without_spaces_is_not_one_token() {
        let a = Analyser::new();
        for sample in ["日本語のテキスト", "ภาษาไทยเป็นภาษา", "中文分词测试"] {
            let tokens = a.tokens(sample);
            assert!(
                tokens.len() > 1,
                "{sample:?} segmented to {tokens:?} — a naive tokeniser's answer"
            );
        }
    }

    /// **Every script this design names produces at least one token**, which the segmenter's own
    /// `is_word_like()` does not deliver: it reports a single-word CJK run and every Khmer run as
    /// not-word-like, so a document containing exactly `中文` would be unfindable by the query
    /// `中文`. This is the regression test for that, and it is a recall test, not a quality one.
    #[test]
    fn no_script_analyses_to_nothing() {
        let a = Analyser::new();
        for sample in [
            "中文",
            "日本語",
            "x 日本語",
            "ភាសាខ្មែរ",
            "မြန်မာဘာသာစကား",
            "ນີ້ແມ່ນພາສາລາວ",
            "ΑΘΗΝΑ",
            "Москва",
            "café",
            "2401.00042",
        ] {
            assert!(
                !a.tokens(sample).is_empty(),
                "{sample:?} analysed to no tokens at all — it would be unfindable"
            );
        }
    }

    /// The mixed-script case the family exists for: one analyser, nothing declared, and **no run
    /// lost at a script boundary**. The CJK halves here are exactly the single-word runs that the
    /// word-like flag drops.
    #[test]
    fn a_mixed_script_field_keeps_every_run() {
        let a = Analyser::new();
        let tokens = a.tokens("Test 日本語 mixed English 中文");
        for expected in ["test", "日本語", "mixed", "english", "中文"] {
            assert!(
                tokens.iter().any(|t| t == expected),
                "{expected:?} is missing from {tokens:?}"
            );
        }
    }
}
