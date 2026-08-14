//! The golden known-answer vectors that pin the analyser.
//!
//! **This is where the conformance suite's independence lives.** The oracle reaches the analyser
//! through the `tessera tokenise` verb rather than reimplementing it, because PyICU wraps ICU4C
//! and its segmentation can diverge from icu4x's — so a second implementation would test two
//! libraries against each other rather than testing Tessera (`records-and-search.md` §4.4). What
//! replaces that independence is these vectors: known answers, checked in, read against the
//! design's rules rather than copied from another library.
//!
//! **A diff here is a rebuild.** Every text index is a function of this token stream, and two
//! analysers disagree by producing *different but individually valid* terms — a mismatch nothing
//! downstream can detect. So a change to any expectation below must move
//! [`tessera_analyse::ANALYSER_VERSION`], which the file records and this test checks, and every
//! text index built under the old value must be rebuilt.

use tessera_analyse::{analyser, Analyser, ANALYSER_NAMES};

/// **Every analyser this binary carries owes a vector set**, and every set names an analyser the
/// binary carries. Either half failing is the shape decision 0070 forbids: a pipeline a column can
/// declare but nothing pins, or a pinned pipeline nothing can declare.
#[test]
fn every_analyser_has_vectors_and_every_vector_set_an_analyser() {
    let doc = vectors();
    let named: Vec<&str> = doc["analysers"]
        .as_array()
        .expect("an analyser array")
        .iter()
        .map(|a| a["name"].as_str().expect("a name"))
        .collect();
    for name in ANALYSER_NAMES {
        assert!(named.contains(name), "{name} ships with no golden vectors");
    }
    for name in &named {
        assert!(
            ANALYSER_NAMES.contains(name),
            "{name} has vectors but is not an analyser this binary carries"
        );
    }
}

/// A declared name this binary does not carry is refused rather than defaulted — falling back
/// would index a column with a pipeline its declaration did not ask for.
#[test]
fn an_unknown_analyser_name_is_refused() {
    for name in ["", "Unicode", "icu", "unicode/icu4x-2.2/p1", "standard"] {
        assert!(
            analyser(name).is_none(),
            "{name:?} resolved to an analyser"
        );
    }
    assert!(analyser("unicode").is_some());
}

fn vectors() -> serde_json::Value {
    serde_json::from_str(include_str!("vectors/golden.json")).expect("the vector file parses")
}

#[test]
fn the_golden_vectors_hold() {
    let doc = vectors();
    let set = &doc["analysers"][0];
    let name = set["name"].as_str().expect("a name");
    let analyser = analyser(name).unwrap_or_else(|| panic!("{name} is not carried"));

    assert_eq!(
        set["identity"].as_str().expect("an identity string"),
        analyser.identity(),
        "the vectors were recorded under a different identity than this binary carries. Either \
         the version moved without re-recording the vectors, or the vectors were edited without \
         moving the version — and the second is the one that silently invalidates every index \
         already built"
    );

    let vectors = set["vectors"].as_array().expect("a vector array");
    assert!(vectors.len() >= 10, "the file lost its vectors");

    let mut families: Vec<&str> = Vec::new();
    for vector in vectors {
        let family = vector["family"].as_str().expect("a family name");
        let input = vector["input"].as_str().expect("an input string");
        let expected: Vec<String> = vector["tokens"]
            .as_array()
            .expect("an expected token array")
            .iter()
            .map(|t| t.as_str().expect("a token string").to_string())
            .collect();
        assert_eq!(
            analyser.tokens(input),
            expected,
            "{family}: {input:?} no longer analyses to its recorded tokens. If this change is \
             intended, move ANALYSER_VERSION and record that every text index needs rebuilding"
        );
        families.push(family);
    }

    // The design names six scripts that need dictionary segmentation and the suite is required to
    // cover Latin, CJK and Thai at minimum; asserting the coverage here stops a future edit from
    // quietly deleting the awkward cases rather than fixing them.
    for required in [
        "latin", "japanese", "chinese", "thai", "lao", "burmese", "khmer", "khmer-phrase",
        "mixed-script", "chinese-single-word",
    ] {
        assert!(
            families.contains(&required),
            "the vectors no longer cover {required:?}"
        );
    }
}

/// **Tokenising is a pure function of the input**, which the fold's merge argument depends on: two
/// layers' postings may be merged only because the same analyser over the same values produces the
/// same terms (§7). A per-instance or per-call difference would make that false.
#[test]
fn two_analysers_agree_and_repeat() {
    let (a, b): (Analyser, Analyser) = (Analyser::new(), Analyser::new());
    for sample in [
        "The quick brown fox",
        "日本語のテキスト",
        "ภาษาไทยเป็นภาษา",
        "Ｔｅｓｔ 日本語 mixed",
        "",
    ] {
        let once = a.tokens(sample);
        assert_eq!(once, a.tokens(sample), "{sample:?} is not repeatable");
        assert_eq!(once, b.tokens(sample), "{sample:?} differs between instances");
    }
}
