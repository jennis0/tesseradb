//! The golden vectors: known answers per script family, recorded in `vectors/golden.json`.
//!
//! The conformance oracle tokenises through `tessera tokenise`, so these vectors are the check on
//! the analyser that does not pass through the analyser. Every text index is a function of the
//! token stream. A changed expectation therefore moves the analyser's version (`UNICODE_VERSION`
//! in `src/lib.rs`), and every text index built under the old version is rebuilt.

use sha2::{Digest, Sha256};
use tessera_analyse::{
    analyser, analyser_with_identity, Analyser, ANALYSER_NAMES, ANALYSER_VECTOR_DIGESTS,
};

fn vector_sets() -> Vec<serde_json::Value> {
    let doc: serde_json::Value =
        serde_json::from_str(include_str!("vectors/golden.json")).expect("the vector file parses");
    doc["analysers"]
        .as_array()
        .expect("an analyser array")
        .clone()
}

fn text<'a>(value: &'a serde_json::Value, field: &str) -> &'a str {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("a string `{field}`"))
}

fn tokens(vector: &serde_json::Value) -> Vec<&str> {
    vector["tokens"]
        .as_array()
        .expect("a token array")
        .iter()
        .map(|t| t.as_str().expect("a token string"))
        .collect()
}

#[test]
fn a_name_this_binary_does_not_carry_is_refused() {
    for name in ["", "Unicode", "icu", "unicode/icu4x-2.2/p1", "standard"] {
        assert!(analyser(name).is_none(), "{name:?} resolved to an analyser");
    }
    assert!(analyser("unicode").is_some());
}

#[test]
fn an_identity_resolves_only_at_the_version_this_binary_carries() {
    let identity = Analyser::new().identity();
    assert!(analyser_with_identity(&identity).is_some());
    for stale in [
        "",
        "unicode",
        "unicode/icu4x-1.0/p1",
        "standard/icu4x-2.2/p1",
    ] {
        assert!(
            analyser_with_identity(stale).is_none(),
            "{stale:?} resolved"
        );
    }
}

/// Every analyser has a vector set and a digest, and every vector set names an analyser.
#[test]
fn analysers_vector_sets_and_digests_name_each_other() {
    let sets = vector_sets();
    let mut named: Vec<&str> = sets.iter().map(|set| text(set, "name")).collect();
    let mut digested: Vec<&str> = ANALYSER_VECTOR_DIGESTS.iter().map(|(n, _)| *n).collect();
    let mut carried = ANALYSER_NAMES.to_vec();
    named.sort_unstable();
    digested.sort_unstable();
    carried.sort_unstable();
    assert_eq!(named, carried, "vector sets against ANALYSER_NAMES");
    assert_eq!(
        digested, carried,
        "ANALYSER_VECTOR_DIGESTS against ANALYSER_NAMES"
    );
}

#[test]
fn the_golden_vectors_hold() {
    for set in vector_sets() {
        let name = text(&set, "name");
        let analyser = analyser(name).unwrap_or_else(|| panic!("{name} is not carried"));
        assert_eq!(
            text(&set, "identity"),
            analyser.identity(),
            "{name}: the vectors were recorded under another identity. Re-record them under the \
             version this binary carries"
        );

        let vectors = set["vectors"].as_array().expect("a vector array");
        for vector in vectors {
            let (family, input) = (text(vector, "family"), text(vector, "input"));
            assert_eq!(
                analyser.tokens(input),
                tokens(vector),
                "{name}/{family}: {input:?} no longer analyses to its recorded tokens. If the \
                 change is intended, move the analyser's version in src/lib.rs and rebuild every \
                 text index"
            );
        }

        // Every analyser covers the six dictionary-segmented scripts and the single-word runs the
        // segmenter's word-like flag drops, so an edit cannot remove the hard cases.
        let families: Vec<&str> = vectors.iter().map(|v| text(v, "family")).collect();
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

/// The form the digest is taken over: name and recorded identity, then each vector's family,
/// input and tokens, unit-separated within a record and record-separated between. The `why` notes
/// are outside it, because a note changes no index.
fn recorded_answers(set: &serde_json::Value) -> String {
    let mut buf = format!("{}\u{1f}{}\u{1e}", text(set, "name"), text(set, "identity"));
    for vector in set["vectors"].as_array().expect("a vector array") {
        buf.push_str(text(vector, "family"));
        buf.push('\u{1f}');
        buf.push_str(text(vector, "input"));
        for token in tokens(vector) {
            buf.push('\u{1f}');
            buf.push_str(token);
        }
        buf.push('\u{1e}');
    }
    buf
}

/// Vectors regenerated from changed behaviour pass `the_golden_vectors_hold` with the version
/// unmoved. They do not pass this: the digest sits beside the version in `src/lib.rs`.
#[test]
fn the_recorded_answers_match_the_digest_beside_the_version() {
    for set in vector_sets() {
        let name = text(&set, "name");
        let (_, expected) = ANALYSER_VECTOR_DIGESTS
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} has vectors but no digest"));
        let digest = format!("{:x}", Sha256::digest(recorded_answers(&set).as_bytes()));
        assert_eq!(
            &digest, expected,
            "{name}: the recorded answers changed. Move the analyser's version in src/lib.rs with \
             this digest, and rebuild every text index built under the old version"
        );
    }
}
