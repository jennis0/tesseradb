//! The text family's analysers: named, versioned pipelines, one declared per `text` column
//! (`records-and-search.md` §4.4, [decision 0070](../../../docs/decisions/0070-analysers-are-named-and-declared-per-column.md)).
//!
//! **One ships today — [`UNICODE`] — and the shape holds more.** A pipeline that is right for a
//! column of abstracts is wrong for a column of stack traces: identifiers split on case and
//! punctuation boundaries that prose must not, and prose wants folding that an identifier must
//! not. The choice belongs to the column, so an analyser is selected by *name* and its full
//! identity is recorded against the column that used it.
//!
//! ⊘ **Not a plugin, and deliberately** (0070 §5). Analysers are built-in variants, because a
//! loaded one would make the token stream a deployment variable and demote the golden vectors from
//! pinning *the* analyser to pinning only a default — and determinism here is load-bearing for I9
//! and for §7's fold-merge argument. If a plugin host ever arrives, a hosted analyser is one more
//! named variant.
//!
//! # `unicode`, the one that ships
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
//! Stemming, stopwords, diacritic folding and synonyms are **deliberately absent from `unicode`**
//! (§9). Each is language-dependent — `ö` and `o` are the same letter in German and different
//! letters in Swedish — each is a conformance surface, and a wrong default corrupts recall
//! silently rather than loudly. Under 0070 that is a statement about *this* analyser rather than
//! about analysers, which is what makes a future stemming pipeline an addition rather than a
//! contradiction: it would be a new name, with its own vectors, declared by the columns that want
//! it.
//!
//! # The identity is part of the artefact
//!
//! [`Analyser::identity`] is recorded in the manifest **against the column that used it**, and
//! changing it is a rebuild of that column — exactly as changing a category's width is. A token
//! stream is not self-describing: an index built under one identity and queried under another
//! would fail to match on precisely the strings whose segmentation differs, which is a silent
//! recall bug rather than an error. The fold's merge argument depends on it too (§7): two layers'
//! postings may be merged only because the same versioned analyser produced them over the same
//! values, which is a per-column check and not a global assumption.
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
use icu_properties::props::{Alphabetic, GeneralCategory, GeneralCategoryGroup};
use icu_properties::{
    CodePointMapData, CodePointMapDataBorrowed, CodePointSetData, CodePointSetDataBorrowed,
};
use icu_segmenter::options::WordBreakInvariantOptions;
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed};

/// The name a `text` column declares to select the general prose pipeline.
pub const UNICODE: &str = "unicode";

/// `unicode`'s version: the data it carries and the shape of its stages, because either changing
/// changes the token stream.
///
/// The `icu4x` component is the crate major-minor whose compiled data this binary carries; the `p`
/// component is the pipeline's own shape, and it moves if a stage is added, removed or reordered
/// even when icu4x does not.
///
/// **This string is a hand-maintained claim about the data behind the token stream**, and the
/// claim is only sound because every input to that stream comes from one pinned place.
/// `Cargo.toml` pins the four icu4x crates at `=2.2`, so a data bump has to be a deliberate edit
/// and the edit has to move this constant with it. A caret range would let `cargo update` change
/// the token stream while leaving the identity untouched — the base build and the next flush
/// disagreeing about where a word ends, with every identity check passing, which is precisely the
/// failure decision 0070 exists to catch and the one thing the identity cannot catch about itself.
///
/// **Nothing in the pipeline consults Rust std's Unicode tables**, and that is deliberate rather
/// than incidental. `Analyser::tokens` decides whether a segment is a token from `Alphabetic ∪
/// General_Category ∈ {Nd, Nl, No}`, taken from `icu_properties` — the same pin as the segmenter's
/// dictionaries. It read `char::is_alphanumeric` until 2026-08-14, which is the same set by
/// definition but from **std's** tables, and those move with the *toolchain*: a code point becoming
/// alphanumeric in a later Unicode revision would have turned a segment that produced no token into
/// one that does, in a corpus using it, with nothing recording the change. Small exposure — new
/// script blocks and numeric forms, never the Latin or CJK cores — but it was the one input to this
/// identity that nothing observed.
///
/// The swap did **not** change the token rule, so `p1` stands and no index needs rebuilding: the
/// two definitions were compared across all 1,114,112 code points at icu4x 2.2 and rustc 1.90 and
/// **agree exactly** — zero disagreements, so no segment classifies differently. (Both candidate
/// formulations were measured: the general-category one above, which is std's own definition of
/// `is_numeric`, and `Alphabetic ∪ Numeric_Type ≠ None`. Both matched, and the first is used
/// because tracking std's definition is what keeps a future divergence a data question rather than
/// a definition question.) A change to the rule itself would be `p1` → `p2` and a rebuild.
const UNICODE_VERSION: &str = "icu4x-2.2/p1";

/// The recorded answers each analyser is pinned to, digested — checked in **beside the version
/// they were recorded under**, which is the point of it being here rather than in the vector file.
///
/// [`Analyser::identity`] is a function of the two constants above and of nothing the tokeniser
/// does, so the identity check in `tests/golden.rs` cannot see the edit its own message calls the
/// dangerous one: vectors regenerated from changed behaviour with the version left where it was.
/// Both copies of the identity string still agree, and every token assertion agrees by
/// construction, because the answers were taken from the new behaviour. This table is the half
/// that does see it — editing `tests/vectors/golden.json` fails here until the digest is moved,
/// and moving it is an edit one line from the version it should have moved with.
///
/// The digest is SHA-256 over the analyser's name, its recorded identity, and each vector's
/// family, input and expected tokens, unit-separated and record-terminated; `tests/golden.rs`
/// holds the canonical form and prints what it computed. The `why` notes are deliberately outside
/// it — recording *why* an answer is right changes no index and is not a rebuild.
///
/// **Every name in [`ANALYSER_NAMES`] owes a row here**, which the same test asserts: a second
/// pipeline arriving without one is the case decision 0070 forbids, a pinned analyser nothing
/// pins.
pub const ANALYSER_VECTOR_DIGESTS: &[(&str, &str)] = &[(
    UNICODE,
    "8f5b0d5efb3708f2f6d2d7a88ad56f163ffb91a03874908c38bd5bcda1eddbbd",
)];

/// Every analyser this binary can be asked for, by name. **A name not in this list is refused** —
/// there is no default fallback, because falling back would index a column with a pipeline its
/// declaration did not ask for, which is the silent-mismatch failure 0070 exists to prevent.
pub const ANALYSER_NAMES: &[&str] = &[UNICODE];

/// Resolve an analyser by declared name.
///
/// `None` for a name this binary does not carry — a caller's error to report, never one to paper
/// over with a default.
pub fn analyser(name: &str) -> Option<Analyser> {
    match name {
        UNICODE => Some(Analyser::new()),
        _ => None,
    }
}

/// `unicode`: the three stages, constructed once and reused.
///
/// **Construction is not free and tokenising is**, which is why this is a type rather than a
/// function: the segmenter's dictionary data is deserialised at construction, and a flush that
/// built one per row would pay that per row. Build one per flush (or per build stage) and pass it
/// down. It holds no request state and is `Send + Sync`.
pub struct Analyser {
    // The compiled-data constructors hand back `'static` borrows of data baked into the binary, so
    // these are handles rather than owned tables — the cost this type exists to amortise is the
    // segmenter's deserialisation, not an allocation.
    // The three are not `Clone` — icu4x's borrowed handles are not — so a holder that must clone
    // itself, as a live generation's columns do per publication, holds this behind an `Arc`.
    nfkc: ComposingNormalizerBorrowed<'static>,
    case: CaseMapperBorrowed<'static>,
    words: WordSegmenterBorrowed<'static>,
    // The token rule's two property lookups, from the same pin as the stages above rather than
    // from std — see [`UNICODE_VERSION`] for why the toolchain must not be an input here.
    alphabetic: CodePointSetDataBorrowed<'static>,
    general_category: CodePointMapDataBorrowed<'static, GeneralCategory>,
}

impl std::fmt::Debug for Analyser {
    /// Named rather than structural: the three stages have no useful `Debug` of their own, and the
    /// identity is the thing a reader of a `FilterColumns` dump actually wants.
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
    /// The identity recorded against every column this analyser indexed — `<name>/<version>`.
    pub fn identity(&self) -> String {
        format!("{UNICODE}/{UNICODE_VERSION}")
    }

    /// The declared name that selects this analyser.
    pub fn name(&self) -> &'static str {
        UNICODE
    }

    pub fn new() -> Self {
        Analyser {
            nfkc: ComposingNormalizer::new_nfkc(),
            case: CaseMapper::new(),
            // `new_auto` picks per script run: dictionary segmentation for Chinese, Japanese, Thai,
            // Lao, Khmer and Burmese, and the plain UAX #29 rules elsewhere. That per-run choice is
            // what lets one analyser serve a mixed-script corpus with nothing declared.
            words: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            alphabetic: CodePointSetData::new::<Alphabetic>(),
            general_category: CodePointMapData::<GeneralCategory>::new(),
        }
    }

    /// Does this character make its segment a token? `Alphabetic ∪ General_Category ∈ {Nd, Nl, No}`
    /// — std's own definition of `char::is_alphanumeric`, evaluated against **icu4x's** tables so
    /// that the pipeline has exactly one Unicode version and this crate's identity covers all of it
    /// ([`UNICODE_VERSION`]).
    fn alphanumeric(&self, c: char) -> bool {
        self.alphabetic.contains(c)
            || GeneralCategoryGroup::Number.contains(self.general_category.get(c))
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
        let mut scratch = TokenScratch::default();
        let mut out = Vec::new();
        self.for_each_token(text, &mut scratch, &mut |token| out.push(token.to_string()));
        out
    }

    /// The same tokens as [`Self::tokens`], **borrowed** rather than owned, over buffers the
    /// caller keeps between documents.
    ///
    /// The two stages before the segmenter each produce a whole second copy of the document, and
    /// [`Self::tokens`] then produces a third, one `String` per token — so a column of 7.4×10⁷
    /// short names costs on the order of 4×10⁸ allocations to index, of which the index keeps a
    /// few per cent: a term seen before is looked up and the freshly allocated key dropped. Here
    /// the normalisation lands in a buffer that is reused for the next document and every token is
    /// a slice of the folded one, so the caller allocates only where it decides to keep something
    /// (`pipeline.rs`'s text index allocates on a term's first sighting alone).
    ///
    /// **The token sequence is [`Self::tokens`]'s exactly** — that function is written in terms of
    /// this one, so there is one segmentation rule rather than two that could drift, and the golden
    /// vectors pin both at once.
    ///
    /// ⊘ The case folder still allocates where a document is not already folded. Writing its
    /// output into a reused buffer needs `writeable::Writeable` in scope, which is a direct
    /// dependency whose version has to track icu4x's own or the trait is a different type; the
    /// NFKC stage has an inherent sink method and takes the buffer.
    pub fn for_each_token(&self, text: &str, scratch: &mut TokenScratch, f: &mut impl FnMut(&str)) {
        // Written unconditionally, where `normalize` returns a `Cow` that borrows an
        // already-normalised input: that borrow costs a full normalising pass into a checking sink
        // to discover, so the branch it saves is a copy, not the work.
        scratch.normalised.clear();
        let _ = self.nfkc.normalize_to(text, &mut scratch.normalised);
        let folded = self.case.fold_string(&scratch.normalised);
        let mut breaks = self.words.segment_str(&folded);
        let Some(mut start) = breaks.next() else {
            return;
        };
        for end in breaks {
            let segment = &folded[start..end];
            if segment.chars().any(|c| self.alphanumeric(c)) {
                f(segment);
            }
            start = end;
        }
    }
}

/// The buffers [`Analyser::for_each_token`] reuses across documents.
///
/// Held by the caller rather than by the [`Analyser`], which is shared across threads and holds no
/// request state — a scratch inside it would have to be a lock or a thread-local, and the callers
/// that want it are already single sweeps with somewhere to put one.
#[derive(Default)]
pub struct TokenScratch {
    normalised: String,
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

    /// **The token rule is a union, and the numeric half is the part `Alphabetic` alone drops.**
    ///
    /// Worth its own test because the property moved from std to `icu_properties` on 2026-08-14
    /// ([`UNICODE_VERSION`]) and the two halves are separate lookups there: a rule that kept only
    /// the first would still tokenise every word in every golden vector and would silently stop
    /// indexing accession numbers, years and every digit-only field.
    ///
    /// Non-Latin digits are the case that separates the union from ASCII-mindedness: `٣٤٥` is
    /// `General_Category = Nd` and not `Alphabetic`, and NFKC leaves it alone, so it reaches the
    /// rule as itself.
    #[test]
    fn a_segment_of_digits_alone_is_a_token() {
        let a = Analyser::new();
        assert_eq!(a.tokens("2026"), vec!["2026"]);
        assert_eq!(a.tokens("٣٤٥"), vec!["٣٤٥"]);
        assert_eq!(a.tokens("accession 2026"), vec!["accession", "2026"]);
    }

    /// **Duplicates and order are kept.** `match` needs neither, but the positional payload the
    /// phrase and scoring upgrades share needs both, and an analyser that deduplicated here would
    /// force them to re-analyse.
    #[test]
    fn duplicates_and_order_survive() {
        let a = Analyser::new();
        assert_eq!(
            a.tokens("the cat the hat"),
            vec!["the", "cat", "the", "hat"]
        );
    }

    /// A script with no inter-word spaces segments into words rather than into one token — the
    /// property that rules out split-on-non-alphanumerics, asserted rather than assumed.
    #[test]
    fn a_script_without_spaces_is_not_one_token() {
        let a = Analyser::new();
        for sample in ["日本語のテキスト", "ภาษาไทยเป็นภาษา", "中文分词测试"]
        {
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
