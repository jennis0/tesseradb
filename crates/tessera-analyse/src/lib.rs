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
    //
    // The first two stages are [`Fold`], which is factored out because a second surface wants them
    // without the segmenter: see that type.
    fold: Fold,
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
            fold: Fold::new(),
            // `new_auto` picks per script run: dictionary segmentation for Chinese, Japanese, Thai,
            // Lao, Khmer and Burmese, and the plain UAX #29 rules elsewhere. That per-run choice is
            // what lets one analyser serve a mixed-script corpus with nothing declared.
            words: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            alphabetic: CodePointSetData::new::<Alphabetic>(),
            general_category: CodePointMapData::<GeneralCategory>::new(),
        }
    }

    /// The first two stages alone — NFKC, then full case folding — for a caller that wants the
    /// pipeline's *fold* without its segmentation.
    ///
    /// **One fold in the codebase, not two.** The suggestion surface
    /// (`docs/design/value-suggestion.md` §4) folds a category key, a title and a typed query by
    /// exactly this rule, and it is the same rule rather than a second one that could drift — a
    /// query folded one way and an indexed entry the other match on nothing, silently.
    pub fn fold(&self) -> &Fold {
        &self.fold
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
        let folded = self.fold.fold_into(text, &mut scratch.normalised);
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

// ---------------------------------------------------------------------------------------------
// The fold, and the suggestion surface's own rules beside it
// ---------------------------------------------------------------------------------------------

/// The `unicode` pipeline's first two stages — **NFKC, then full Unicode case folding** — with no
/// segmenter.
///
/// Factored out of [`Analyser`] so that a surface wanting the fold and not the token stream calls
/// the same code rather than restating the rule. There is one fold in this repository; a second
/// spelling of it would mean a query folded one way and an indexed string the other, matching on
/// nothing with no error anywhere.
///
/// The order is [`Analyser`]'s and is load-bearing for the same reason: normalising first means the
/// case folder sees one spelling of each character.
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
        let mut scratch = String::new();
        self.fold_into(text, &mut scratch).into_owned()
    }

    /// [`Self::fold`] with the *normalisation* stage written into a buffer the caller keeps.
    ///
    /// The normalised copy is a whole second copy of the input, so a sweep over many strings holds
    /// one buffer rather than allocating per string. ⊘ The case folder still allocates its own
    /// output: writing it into a reused buffer needs `writeable::Writeable` in scope, which is a
    /// direct dependency whose version has to track icu4x's own.
    pub fn fold_into<'a>(&self, text: &str, normalised: &'a mut String) -> Cow<'a, str> {
        // Written unconditionally, where `normalize` returns a `Cow` that borrows an
        // already-normalised input: that borrow costs a full normalising pass into a checking sink
        // to discover, so the branch it saves is a copy, not the work.
        normalised.clear();
        let _ = self.nfkc.normalize_to(text, normalised);
        self.case.fold_string(&*normalised)
    }
}

/// **The suggestion surface's own rules** — whitespace collapse and the word-boundary rule —
/// beside the fold they run after (`docs/design/value-suggestion.md` §4).
///
/// These are *not* the analyser's. They decide what entries a category vocabulary's suggestion
/// index holds and say nothing about how a `text` column is tokenised: [`Analyser`] segments by
/// UAX #29 and never collapses whitespace, and neither behaviour changes because this type exists.
/// The two share exactly one thing, [`Fold`], and share it so that a typed query and an indexed
/// key fold identically.
///
/// **A word boundary is a transition into a letter or digit from anything else, after folding** —
/// `Alphabetic ∪ General_Category ∈ {Nd, Nl, No}`, the same union [`Analyser`]'s token rule uses
/// and from the same pinned tables, so this crate has one Unicode version and
/// [`Analyser::identity`] covers all of it.
///
/// **A script written without spaces yields no word-start entries.** The rule is a transition into
/// a letter from something that is not one, so a run of Han, Khmer or Thai written with no
/// separators is one word however many words a reader sees in it — such a vocabulary is suggested
/// on whole-key and whole-title prefixes alone. This is a deliberate consequence of using a
/// boundary rule rather than the segmenter: a dictionary segmentation would multiply the index by
/// the character count for those scripts and would make an entry set a function of icu4x's data
/// version rather than of the string.
pub struct SuggestionFold {
    fold: Fold,
    alphabetic: CodePointSetDataBorrowed<'static>,
    general_category: CodePointMapDataBorrowed<'static, GeneralCategory>,
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

impl SuggestionFold {
    pub fn new() -> Self {
        SuggestionFold {
            fold: Fold::new(),
            alphabetic: CodePointSetData::new::<Alphabetic>(),
            general_category: CodePointMapData::<GeneralCategory>::new(),
        }
    }

    /// The shared fold, for a caller that wants it without the two rules above.
    pub fn fold(&self) -> &Fold {
        &self.fold
    }

    /// **The entry string for `text`**: folded, then runs of Unicode whitespace collapsed to one
    /// space, then trimmed.
    ///
    /// This is applied to the query and to every indexed string alike, so a match is an equality of
    /// folded bytes and never a judgement. The empty string is an ordinary answer — a value whose
    /// key is whitespace alone cannot exist (the ingest wire refuses an empty value), but a *title*
    /// of whitespace is a shape an author can write, and it indexes nothing rather than refusing.
    pub fn entry(&self, text: &str) -> String {
        collapse_whitespace(&self.fold.fold(text))
    }

    /// [`Self::entry`] with the normalisation buffer reused across strings.
    pub fn entry_into(&self, text: &str, normalised: &mut String) -> String {
        let folded = self.fold.fold_into(text, normalised);
        collapse_whitespace(&folded)
    }

    /// Is this character part of a word — `Alphabetic ∪ Number ∪ Mark`?
    ///
    /// The first two are [`Analyser`]'s token rule exactly. **`Mark` is this rule's own addition
    /// and it is not decoration**: a combining mark is not `Alphabetic`, so without it the Thai
    /// `เป็น` breaks at U+0E47 and the letter after it reads as a new word — and so does every
    /// Devanagari, Thai, Arabic or Hebrew word carrying a diacritic. That would put spurious
    /// word-start entries in the index for exactly the scripts a whole-word rule serves worst. A
    /// mark never begins a word in well-formed text, so admitting it can only *join* runs.
    fn wordish(&self, c: char) -> bool {
        let category = self.general_category.get(c);
        self.alphabetic.contains(c)
            || GeneralCategoryGroup::Number.contains(category)
            || GeneralCategoryGroup::Mark.contains(category)
    }

    /// The **byte offsets of every word start after the first**, in an already-entry-folded string.
    ///
    /// The first word is not a word *start* entry — it is the whole-string entry, which the index
    /// holds under its own kind — so it is excluded here rather than filtered by every caller.
    ///
    /// **"The first" is the first *word*, not the byte at offset 0.** An entry opening with a
    /// non-word character — `(cs.lg)`, `[draft] machine learning` — has its first word at a
    /// non-zero offset, and treating offset 0 as the only exclusion would count that word as a
    /// start. That is not merely one entry too many: [`Self::served_word_starts`] applies the rule
    /// below, so the two lists would differ in length and [`Self::entries_of`] would drop **every**
    /// word start of such a value. The entry fold trims leading whitespace and nothing else, so a
    /// leading bracket survives into the entry string and this case is ordinary rather than exotic.
    pub fn word_starts(&self, entry: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut previous_wordish = false;
        let mut leading = true;
        for (at, c) in entry.char_indices() {
            let wordish = self.wordish(c);
            if wordish && !previous_wordish {
                if leading {
                    leading = false;
                } else {
                    out.push(at);
                }
            }
            previous_wordish = wordish;
        }
        out
    }

    /// The **character offsets** of the same boundaries in a string that has *not* been folded —
    /// the key or the title as an author wrote it, which is the string a response serves.
    ///
    /// Two lists rather than one map because the fold has no offset-preserving form: icu4x
    /// normalises and folds whole strings, so nothing can say which source character a folded byte
    /// came from. What makes the pairing sound instead is *where* the boundaries are: a word start
    /// is a transition **into** a letter or digit, so the character before one is neither, and NFKC
    /// never composes across such a character. Folding from a word start therefore gives exactly
    /// the tail of the whole string's fold, and the k-th boundary in the served string is the k-th
    /// in the folded one.
    ///
    /// **Where the two lists differ in length the caller must index no word starts for that
    /// value**, and [`Self::entries_of`] does. NFKC can move a boundary — `¼` becomes `1⁄4`, one
    /// word where there was one non-word character — and a pairing that assumed equal counts would
    /// record an offset into the wrong word. It is rare enough that dropping the word starts of the
    /// values it happens to is cheaper than any machinery for getting it right.
    ///
    /// **Equal counts are a necessary check and not a proof, and the difference is stated because
    /// it will not be rediscovered.** Two boundary changes that cancel leave the counts equal and
    /// the pairing wrong: `a㎏b ¼` folds to `akgb 1⁄4` — the `㎏` splits `a…b` into two words where
    /// the served string had one, and the `¼` adds another, so both lists come to the same length
    /// over different boundaries. A cheap total check cannot see that; only a fold with an offset
    /// map could, and icu4x has none.
    ///
    /// What bounds the damage is *where* the offset is used. It reaches `match.start` and
    /// `match.len` and nothing else — a highlight drawn over the wrong characters of a string the
    /// client was going to draw anyway. It is not an input to the entry string (which is the fold's
    /// own output and is right either way), so **a mis-paired offset cannot change which values
    /// match**, and it is not an input to anything the gate reads, so it cannot change which values
    /// are served. A wrong highlight, never a wrong disclosure.
    pub fn served_word_starts(&self, served: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut previous_wordish = false;
        let mut leading = true;
        for (index, c) in served.chars().enumerate() {
            // Leading whitespace is trimmed out of the entry string, so a served string that opens
            // with it has its first word at a non-zero character index and that word is the
            // *whole-string* entry rather than a word start. Everything before the first wordish
            // character is therefore skipped here on the same rule the entry fold applies.
            let wordish = self.wordish(c);
            if wordish && !previous_wordish {
                if leading {
                    leading = false;
                } else {
                    out.push(index);
                }
            }
            previous_wordish = wordish;
        }
        out
    }

    /// The character index the whole-string entry starts at in `served` — its leading whitespace,
    /// which the entry fold trims.
    pub fn served_start(&self, served: &str) -> usize {
        served
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0)
    }
}

/// Which served string an entry came from — `key` or `title`, as `match.field` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SuggestionField {
    Key,
    Title,
}

impl SuggestionField {
    /// The wire spelling, which is the field name in the response and nothing else.
    pub fn as_str(self) -> &'static str {
        match self {
            SuggestionField::Key => "key",
            SuggestionField::Title => "title",
        }
    }
}

/// The three entry kinds, **in the order ties between them break**
/// (`docs/design/value-suggestion.md` §7): a whole-key match precedes a whole-title match, which
/// precedes a word-start match derived from a longer string.
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

/// One `(folded entry string, where it came from)` pair — what the suggestion index holds a run of
/// per distinct entry string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionEntry {
    /// The folded, whitespace-collapsed, trimmed string a prefix is matched against.
    pub entry: String,
    pub kind: EntryKind,
    pub field: SuggestionField,
    /// The **character** offset into the served string (`key` or `title` as an author wrote it)
    /// this entry begins at — what `match.start` reports, and where a response re-folds forward
    /// from to derive `match.len`.
    pub start: u32,
}

impl SuggestionFold {
    /// **Every entry one value contributes** (`docs/design/value-suggestion.md` §4): the whole key,
    /// the whole title where an author wrote one, and every word start after the first of the title
    /// — or of the key, where there is no title, which is what "a value with no title indexes its
    /// key as the title would be" means.
    ///
    /// An entry that folds to the empty string is dropped rather than indexed: it would match every
    /// prefix range's lower bound and stands for nothing a caller could have typed.
    ///
    /// Word starts are dropped for a value whose served and folded boundary counts disagree — see
    /// [`Self::served_word_starts`] for the case and why dropping is the answer.
    pub fn entries_of(&self, key: &str, title: Option<&str>) -> Vec<SuggestionEntry> {
        let mut out = Vec::new();
        let mut scratch = String::new();

        let key_entry = self.entry_into(key, &mut scratch);
        if !key_entry.is_empty() {
            out.push(SuggestionEntry {
                entry: key_entry,
                kind: EntryKind::Key,
                field: SuggestionField::Key,
                start: self.served_start(key) as u32,
            });
        }

        let (word_source, field) = match title {
            Some(title) => {
                let title_entry = self.entry_into(title, &mut scratch);
                if !title_entry.is_empty() {
                    out.push(SuggestionEntry {
                        entry: title_entry,
                        kind: EntryKind::Title,
                        field: SuggestionField::Title,
                        start: self.served_start(title) as u32,
                    });
                }
                (title, SuggestionField::Title)
            }
            None => (key, SuggestionField::Key),
        };

        let folded = self.entry_into(word_source, &mut scratch);
        let folded_starts = self.word_starts(&folded);
        let served_starts = self.served_word_starts(word_source);
        if folded_starts.len() == served_starts.len() {
            for (at, served) in folded_starts.into_iter().zip(served_starts) {
                out.push(SuggestionEntry {
                    entry: folded[at..].to_string(),
                    kind: EntryKind::WordStart,
                    field,
                    start: served as u32,
                });
            }
        }
        out
    }
}

/// Runs of Unicode whitespace collapsed to one space, and the result trimmed.
///
/// `str::split_whitespace` is Unicode's `White_Space` property from **std's** tables, which move
/// with the toolchain — acceptable here and not in the token rule, because whitespace is not part
/// of any indexed identity: a code point becoming whitespace in a later revision would change how
/// one value's title collapses, which is a rebuild of a derived index and never a stored ordinal.
/// [`UNICODE_VERSION`] covers the fold, which is the part an artefact records.
pub fn collapse_whitespace(folded: &str) -> String {
    let mut out = String::with_capacity(folded.len());
    for word in folded.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
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

    /// **The factored fold is the analyser's own first two stages**, not a second spelling of them.
    /// If they ever diverge a typed query and an indexed key stop matching, silently — so the
    /// property is asserted over the pairs whose equality the pipeline's order buys.
    #[test]
    fn the_factored_fold_is_the_analysers_own() {
        let a = Analyser::new();
        let fold = Fold::new();
        for text in ["ﬁle", "ＦＵＬＬ", "İstanbul", "STRASSE", "Ω", "cs.LG", "Machine Learning"] {
            assert_eq!(
                a.fold().fold(text),
                fold.fold(text),
                "{text:?}: the analyser's fold and a free one disagree"
            );
            // And the token stream is still produced from that fold, so the two cannot drift
            // without this failing too.
            let folded = fold.fold(text);
            assert_eq!(a.tokens(text), a.tokens(&folded));
        }
    }

    /// The entry rule is fold, then collapse, then trim — asserted on one string that exercises
    /// all three at once.
    #[test]
    fn an_entry_is_folded_collapsed_and_trimmed() {
        let s = SuggestionFold::new();
        assert_eq!(s.entry("  Machine\t\n  LEARNING  "), "machine learning");
        assert_eq!(s.entry("cs.LG"), "cs.lg");
        assert_eq!(s.entry("ﬁLE"), "file");
        assert_eq!(s.entry("ＦＵＬＬ　width"), "full width");
        assert_eq!(s.entry("   "), "", "whitespace alone is an entry of nothing");
    }

    /// **A word boundary is a transition into a letter or digit**, so an underscore, a dot and a
    /// space all start the next word and a run of letters does not.
    #[test]
    fn word_starts_are_transitions_into_a_letter_or_digit() {
        let s = SuggestionFold::new();
        let entry = s.entry("machine_learning 2026.v2");
        assert_eq!(entry, "machine_learning 2026.v2");
        let starts: Vec<&str> = s
            .word_starts(&entry)
            .into_iter()
            .map(|at| &entry[at..])
            .collect();
        assert_eq!(starts, vec!["learning 2026.v2", "2026.v2", "v2"]);
    }

    /// **A value whose string opens with a non-word character keeps its word starts.**
    ///
    /// `word_starts` excluded only byte 0, where `served_word_starts` skipped the whole leading
    /// non-word run; the two then disagreed by one for `(cs.LG)` and `[Draft] Machine Learning`,
    /// and [`SuggestionFold::entries_of`]'s length check dropped **every** word start of such a
    /// value rather than one. Found in review, and it is not an exotic shape: a bracketed prefix
    /// on a title is ordinary and the entry fold trims only whitespace.
    #[test]
    fn a_leading_non_word_character_does_not_make_the_first_word_a_start() {
        let s = SuggestionFold::new();
        for (served, want) in [
            ("(cs.LG)", vec!["lg)"]),
            ("[Draft] Machine Learning", vec!["machine learning", "learning"]),
            ("  ...cs.LG", vec!["lg"]),
        ] {
            let entry = s.entry(served);
            let starts: Vec<&str> = s
                .word_starts(&entry)
                .into_iter()
                .map(|at| &entry[at..])
                .collect();
            assert_eq!(starts, want, "{served:?} folded to {entry:?}");
            assert_eq!(
                s.word_starts(&entry).len(),
                s.served_word_starts(served).len(),
                "{served:?}: the folded and served boundary lists must agree in length, or \
                 entries_of drops every word start"
            );
            // And the entries actually reach the index, which is the property the length check
            // silently removed.
            let entries = s.entries_of("k", Some(served));
            assert_eq!(
                entries
                    .iter()
                    .filter(|e| e.kind == EntryKind::WordStart)
                    .count(),
                want.len(),
                "{served:?}"
            );
        }
    }

    /// **A script written without spaces yields no word starts** — the consequence §4 names, which
    /// is invisible to an English-language reader and would otherwise be discovered as a missing
    /// feature rather than as a stated rule.
    #[test]
    fn a_script_without_spaces_has_no_word_starts() {
        let s = SuggestionFold::new();
        for sample in ["中文分词测试", "日本語のテキスト", "ภาษาไทยเป็นภาษา"] {
            let entry = s.entry(sample);
            assert!(
                s.word_starts(&entry).is_empty(),
                "{sample:?} produced word starts"
            );
        }
    }

    /// The three entry kinds, in the order §7 breaks ties in, and `start` in characters of the
    /// **served** string rather than of the folded one.
    #[test]
    fn a_value_contributes_a_key_a_title_and_its_word_starts() {
        let s = SuggestionFold::new();
        let entries = s.entries_of("cs.LG", Some("Machine Learning"));
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.entry.as_str(), e.kind, e.field, e.start))
                .collect::<Vec<_>>(),
            vec![
                ("cs.lg", EntryKind::Key, SuggestionField::Key, 0),
                ("machine learning", EntryKind::Title, SuggestionField::Title, 0),
                ("learning", EntryKind::WordStart, SuggestionField::Title, 8),
            ]
        );
    }

    /// A value with no title indexes its **key**'s word starts, which is what "as the title would
    /// be" means — and does not manufacture a title entry.
    #[test]
    fn a_value_with_no_title_takes_its_word_starts_from_its_key() {
        let s = SuggestionFold::new();
        let entries = s.entries_of("machine_learning", None);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.entry.as_str(), e.kind, e.field, e.start))
                .collect::<Vec<_>>(),
            vec![
                ("machine_learning", EntryKind::Key, SuggestionField::Key, 0),
                ("learning", EntryKind::WordStart, SuggestionField::Key, 8),
            ]
        );
    }

    /// **`start` indexes the served string, so leading whitespace and non-ASCII characters shift
    /// it** — a byte offset into the folded form would highlight the wrong characters of the string
    /// the client draws.
    #[test]
    fn start_is_a_character_offset_into_the_served_string() {
        let s = SuggestionFold::new();
        let entries = s.entries_of("k", Some("  Café Noir"));
        let title = entries
            .iter()
            .find(|e| e.kind == EntryKind::Title)
            .expect("a title entry");
        assert_eq!(title.start, 2, "the two leading spaces are trimmed out");
        let word = entries
            .iter()
            .find(|e| e.kind == EntryKind::WordStart)
            .expect("a word-start entry");
        assert_eq!(word.entry, "noir");
        // 'C','a','f','é' are four characters and five bytes; the offset counts characters.
        assert_eq!(word.start, 7);
    }

    /// Where NFKC moves a boundary the served and folded counts disagree, and the value's word
    /// starts are dropped rather than paired wrongly. `¼` folds to `1⁄4` — one non-word character
    /// becoming a word, a digit and another word.
    #[test]
    fn a_value_whose_fold_moves_a_boundary_indexes_no_word_starts() {
        let s = SuggestionFold::new();
        let entries = s.entries_of("k", Some("a¼b"));
        assert!(
            entries.iter().all(|e| e.kind != EntryKind::WordStart),
            "{entries:?}"
        );
        // The whole-title entry still stands, so the value is still suggestible.
        assert!(entries.iter().any(|e| e.kind == EntryKind::Title));
    }

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
