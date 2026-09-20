//! The golden vectors: the expected tokens for sample text in each script family, stored in
//! `vectors/golden.json`.
//!
//! The conformance oracle tokenises by running `tessera tokenise`, so it cannot detect a wrong
//! analyser. These vectors can. Every text index is built from the analyser's tokens, so if an
//! expected answer changes, change the analyser's version (`UNICODE_VERSION` in `src/lib.rs`) and
//! rebuild every text index built under the old version.

use sha2::{Digest, Sha256};
use tessera_analyse::{
    analyser, analyser_with_identity, identity_of, Analyser, ANALYSER_NAMES, ANALYSER_VECTOR_DIGESTS,
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
fn an_unknown_analyser_name_is_refused() {
    for name in ["", "Unicode", "icu", "unicode/icu4x-2.2/p1", "standard"] {
        assert!(analyser(name).is_none(), "{name:?} resolved to an analyser");
    }
    assert!(analyser("unicode").is_some());
    for name in ANALYSER_NAMES {
        let built = analyser(name).expect("a carried name resolves");
        assert_eq!(identity_of(name), Some(built.identity()), "{name}");
    }
    assert_eq!(identity_of("standard"), None);
}

#[test]
fn an_identity_resolves_only_at_this_binarys_version() {
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

/// Every analyser has a vector set and a digest, and every vector set belongs to an analyser.
#[test]
fn analysers_vector_sets_and_digests_correspond() {
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
        let analyser = analyser(name).unwrap_or_else(|| panic!("this binary has no analyser named {name}"));
        assert_eq!(
            text(&set, "identity"),
            analyser.identity(),
            "{name}: the vector file records a different analyser version. Update its `identity` \
             after checking every expected answer against this version"
        );

        let vectors = set["vectors"].as_array().expect("a vector array");
        for vector in vectors {
            let (family, input) = (text(vector, "family"), text(vector, "input"));
            assert_eq!(
                analyser.tokens(input),
                tokens(vector),
                "{name}/{family}: the tokens of {input:?} differ from the expected answer. If the \
                 change is intended, change the analyser's version in src/lib.rs and rebuild \
                 every text index"
            );
        }

        // Each analyser's vectors must include the six scripts that need dictionary segmentation
        // and the single-word runs for which the segmenter's word-like flag is false. Removing one
        // of these cases from the file fails here.
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

/// The bytes that are hashed: the analyser's name and identity, then each vector's family, input
/// and tokens. Fields are separated by U+001F and records end with U+001E. The `why` notes are
/// left out, because editing a note does not change any index.
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

/// If the vectors are regenerated from changed behaviour, `the_golden_vectors_hold` still passes.
/// This test fails until the digest in `src/lib.rs`, next to the version, is updated.
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
            "{name}: the expected answers changed. Change the analyser's version in src/lib.rs, \
             update this digest, and rebuild every text index built under the old version"
        );
    }
}
