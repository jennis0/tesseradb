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

use sha2::{Digest, Sha256};
use tessera_analyse::{analyser, Analyser, ANALYSER_NAMES, ANALYSER_VECTOR_DIGESTS};

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
        assert!(analyser(name).is_none(), "{name:?} resolved to an analyser");
    }
    assert!(analyser("unicode").is_some());
}

fn vectors() -> serde_json::Value {
    serde_json::from_str(include_str!("vectors/golden.json")).expect("the vector file parses")
}

/// **Every recorded answer still holds, for every analyser the file carries** — not only the
/// first. `ANALYSER_NAMES` holds one name today, so this loop checks one set; it exists because
/// the day decision 0070's second pipeline arrives is the day this file is least likely to be
/// re-read, and a set nothing iterates to is a set nothing checks.
///
/// Mutations this kills: a tokeniser change absorbed into any analyser's expectations rather than
/// into its version (through [`the_recorded_answers_match_the_digest_beside_the_version`]); a
/// second analyser shipped with wrong vectors under a name the coverage test says is covered.
#[test]
fn the_golden_vectors_hold() {
    let doc = vectors();
    let sets = doc["analysers"].as_array().expect("an analyser array");
    assert!(!sets.is_empty(), "the file lost its analysers");
    for set in sets {
        let name = set["name"].as_str().expect("a name");
        let analyser = analyser(name).unwrap_or_else(|| panic!("{name} is not carried"));

        assert_eq!(
            set["identity"].as_str().expect("an identity string"),
            analyser.identity(),
            "{name}: the vectors were recorded under a different identity than this binary \
             carries. Either the version moved without re-recording the vectors, or the vectors \
             were edited without moving the version — the second is the one that silently \
             invalidates every index already built, and it is the digest beside the version, not \
             this assertion, that catches it"
        );

        let vectors = set["vectors"].as_array().expect("a vector array");
        assert!(vectors.len() >= 10, "{name}: the file lost its vectors");

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
                "{name}/{family}: {input:?} no longer analyses to its recorded tokens. If this \
                 change is intended, move ANALYSER_VERSION and record that every text index needs \
                 rebuilding"
            );
            families.push(family);
        }

        // The design names six scripts that need dictionary segmentation and the suite is
        // required to cover Latin, CJK and Thai at minimum; asserting the coverage here stops a
        // future edit from quietly deleting the awkward cases rather than fixing them. Every
        // analyser owes them, not just the first — a second pipeline that indexed the easy
        // scripts and dropped the rest would be pinned by a set that never mentioned them.
        for required in [
            "latin",
            "japanese",
            "chinese",
            "thai",
            "lao",
            "burmese",
            "khmer",
            "khmer-phrase",
            "mixed-script",
            "chinese-single-word",
        ] {
            assert!(
                families.contains(&required),
                "{name}: the vectors no longer cover {required:?}"
            );
        }
    }
}

/// The canonical form the digest is taken over: name, recorded identity, then each vector's
/// family, input and expected tokens — unit-separated within a record, record-separated between.
///
/// The `why` notes are outside it on purpose (see [`ANALYSER_VECTOR_DIGESTS`]): a note recording
/// why an answer is right changes no index, and a digest that fired on prose would train the next
/// reader to move it without thinking.
fn recorded_answers(set: &serde_json::Value) -> String {
    let mut buf = String::new();
    buf.push_str(set["name"].as_str().expect("a name"));
    buf.push('\u{1f}');
    buf.push_str(set["identity"].as_str().expect("an identity string"));
    buf.push('\u{1e}');
    for vector in set["vectors"].as_array().expect("a vector array") {
        buf.push_str(vector["family"].as_str().expect("a family name"));
        buf.push('\u{1f}');
        buf.push_str(vector["input"].as_str().expect("an input string"));
        for token in vector["tokens"].as_array().expect("a token array") {
            buf.push('\u{1f}');
            buf.push_str(token.as_str().expect("a token string"));
        }
        buf.push('\u{1e}');
    }
    buf
}

/// **The answers cannot be edited without an edit beside the version they were recorded under.**
///
/// This is the assertion the identity comparison above cannot make. `Analyser::identity` is built
/// from `UNICODE` and `UNICODE_VERSION` and knows nothing about the tokeniser, so regenerating
/// `vectors/golden.json` from changed behaviour leaves both sides of that comparison identical and
/// every token assertion true — the case the file's own message calls *"the one that silently
/// invalidates every index already built"*. What that edit cannot leave alone is this digest, and
/// the digest lives one line from `UNICODE_VERSION`, so the edit that repairs it is the edit that
/// should have moved the version.
///
/// Mutations this kills: an expected token list rewritten to match a changed tokeniser; a vector
/// deleted, added or reordered; a recorded identity edited in the file; all four with the version
/// left where it was.
#[test]
fn the_recorded_answers_match_the_digest_beside_the_version() {
    let doc = vectors();
    let sets = doc["analysers"].as_array().expect("an analyser array");
    assert!(!sets.is_empty(), "the file lost its analysers");

    for name in ANALYSER_NAMES {
        assert!(
            ANALYSER_VECTOR_DIGESTS.iter().any(|(n, _)| n == name),
            "{name} ships with no recorded-answer digest, so its vectors could be rewritten \
             without an edit beside its version"
        );
    }

    for set in sets {
        let name = set["name"].as_str().expect("a name");
        let (_, expected) = ANALYSER_VECTOR_DIGESTS
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} has vectors but no digest"));
        let digest = format!("{:x}", Sha256::digest(recorded_answers(set).as_bytes()));
        assert_eq!(
            &digest, expected,
            "{name}: the recorded answers no longer digest to ANALYSER_VECTOR_DIGESTS. If the \
             tokeniser changed, this file's answers were regenerated from the new behaviour and \
             ANALYSER_VERSION must move with them; if only the answers were meant to change, say \
             so by moving the version too — every text index built under the old one is stale \
             either way"
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
        assert_eq!(
            once,
            b.tokens(sample),
            "{sample:?} differs between instances"
        );
    }
}
