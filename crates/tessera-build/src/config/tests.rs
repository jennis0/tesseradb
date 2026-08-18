//! The declaration surface's tests, and one of them is the surface itself.
//!
//! [`the_accepted_key_set_is_configuration_ms_table`] asserts the **exact** set of keys each block
//! accepts, read out of serde's own `deny_unknown_fields` message rather than restated by hand — so
//! it fails both when a key documented in `configuration.md` §1 disappears and when one that
//! document does not name appears. That second half is the point: closure is what the leak register
//! rests on, and a key added without an entry in §1 is a disclosure control nobody has reasoned
//! about.

use super::*;

/// Parse `text` as a config file, through a private temporary directory.
///
/// **A fresh `tempdir` per call, not a name derived from the input.** An earlier version named the
/// file after the string's heap address (`{:p}`), which the allocator reuses: two cases running in
/// parallel could land on one path, and a case could read the file another had written. It
/// presented as a *refusal that did not fire* — the parse succeeded against stale bytes — which is
/// the most misleading way for a test helper to fail, since the assertion it breaks is the one
/// asserting something is refused.
fn parse_str(text: &str) -> Result<Config> {
    parse_bound(text, &HashMap::new())
}

fn parse_bound(text: &str, overrides: &HashMap<String, PathBuf>) -> Result<Config> {
    let dir = tempfile::tempdir().expect("tempdir");
    parse_at(dir.path(), text, overrides)
}

/// Parse `text` as a config written into `dir`, so a case can assert **where** a relative `source`
/// resolved to. Sources are paths relative to the declaring document (`configuration.md` §3), so
/// the directory is part of the answer rather than incidental to it.
fn parse_at(dir: &Path, text: &str, overrides: &HashMap<String, PathBuf>) -> Result<Config> {
    let path = dir.join("config.toml");
    std::fs::write(&path, text).expect("write config");
    Config::parse(&path, overrides)
}

fn err(text: &str) -> String {
    format!("{}", parse_str(text).expect_err("expected a refusal"))
}

/// One view, one closed vocabulary, one category over it.
const SEVERITY: &str = r#"
[[view]]
name             = "s0"
extent           = "auto"
point_visibility = { field = "categories", default = "public" }

[[vocabulary]]
name       = "severity"
title      = "Severity"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  low = 1
  high = 2

[[attribute]]
name       = "severity"
title      = "Severity"
type       = "category"
vocabulary = "severity"
render     = true
"#;

/// One layer over `SEVERITY`'s view, with every required control declared.
const LAYER: &str = r#"
[[layer]]
name                      = "clusters/a"
title                     = "clusters"
views                     = ["s0"]
membership                = "enumerated"
hierarchy                 = { kind = "flat", prune_children = true }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }

  [layer.content]
  computed = ["centroid", "box"]
"#;

fn with_layer(extra: &str) -> String {
    format!("{SEVERITY}{LAYER}{extra}")
}

/// `base` with `line` added to its first `[[attribute]]` table.
///
/// **Inserted at a structural landmark, not by matching a formatted line.** A case that built its
/// input with `str::replace` on a placement line silently stopped substituting when the constant's
/// alignment changed — and a case asserting that something is *refused* then passes plain
/// `SEVERITY`, which is accepted, so the no-op presents as the refusal failing to fire rather than
/// as a broken fixture. Panics if the landmark is gone, which is the whole point: a fixture that
/// cannot build its input must fail loudly, not test nothing.
fn with_line(base: &str, line: &str) -> String {
    const AFTER: &str = "[[attribute]]\n";
    assert!(
        base.contains(AFTER),
        "the fixture no longer contains an [[attribute]] table header"
    );
    base.replacen(AFTER, &format!("{AFTER}{line}\n"), 1)
}

// ---------------------------------------------------------------------------------------------
// §1: the surface is a closed set, and this is the assertion that keeps it one
// ---------------------------------------------------------------------------------------------

/// The keys serde reports as accepted for the block a bogus key was planted in.
///
/// `deny_unknown_fields` renders as "unknown field `x`, expected one of `a`, `b`" — or "expected
/// `a`" for a one-field block — so the parser's own field list is recoverable without a second,
/// hand-written copy of it that could drift from the derive.
fn accepted_keys(text: &str) -> Vec<String> {
    let message = err(text);
    let (_, tail) = message
        .split_once("expected ")
        .unwrap_or_else(|| panic!("not an unknown-field refusal: {message}"));
    let mut keys: Vec<String> = tail
        .split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.to_string())
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

fn expect_keys(text: &str, block: &str, documented: &[&str]) {
    let mut want: Vec<String> = documented.iter().map(|s| s.to_string()).collect();
    want.sort();
    let got = accepted_keys(text);
    assert_eq!(
        got, want,
        "{block} accepts a different key set from configuration.md §1's table. Any difference is \
         a change to the declaration surface: a key here that §1 does not name is a control \
         nobody has reasoned about, and one §1 names that is missing is a control an author \
         cannot set"
    );
}

/// **`configuration.md` §1's table, transcribed — and the transcription is the test.**
///
/// One block §1 names is **parsed and not built**: `[layer.labels]`, the sugar expanding to a
/// second layer. It is accepted by the derive *so that the refusal can name it* — decision 0013's
/// rule, since "unknown field `labels`" reads as a typo where a caller who wrote one had every
/// reason to expect it to work, §1 listing it.
#[test]
fn the_accepted_key_set_is_configuration_ms_table() {
    expect_keys(
        "nonesuch = 1\n",
        "the document",
        &["corpus", "view", "vocabulary", "attribute", "layer"],
    );
    expect_keys(
        "[corpus]\nnonesuch = 1\n",
        "[corpus]",
        &["source", "fields"],
    );
    expect_keys(
        "[[view]]\nname = \"s0\"\nnonesuch = 1\n",
        "[[view]]",
        &[
            "name",
            "title",
            "source",
            "fields",
            "extent",
            "point_visibility",
            "visibility",
        ],
    );
    expect_keys(
        "[[view]]\nname = \"s0\"\nextent = { nonesuch = 1 }\n",
        "extent",
        &["auto", "margin", "min", "max", "x", "y"],
    );
    expect_keys(
        "[[view]]\nname = \"s0\"\npoint_visibility = { nonesuch = 1 }\n",
        "point_visibility",
        &["field", "source", "default"],
    );
    expect_keys(
        "[[vocabulary]]\nname = \"v\"\nnonesuch = 1\n",
        "[[vocabulary]]",
        &[
            "name",
            "title",
            "width",
            "value_set",
            "visibility",
            "source",
            "fields",
            "values",
            "reserved",
        ],
    );
    expect_keys(
        "[[attribute]]\nname = \"a\"\nnonesuch = 1\n",
        "[[attribute]]",
        &[
            "name",
            "title",
            "field",
            "type",
            "vocabulary",
            "render",
            "index",
            "multi",
            "render_in",
            "analyser",
        ],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\nnonesuch = 1\n",
        "[[layer]]",
        &[
            "name",
            "title",
            "views",
            "source",
            "fields",
            "artifacts",
            "membership",
            "hierarchy",
            "visibility",
            "artifact_visibility",
            "require_member_visibility",
            "withdraw_on_member_deletion",
            "depends_on",
            "levels",
            "content",
            "members",
            "labels",
        ],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\nhierarchy = { nonesuch = 1 }\n",
        "hierarchy",
        &["kind", "prune_children"],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\nartifact_visibility = { nonesuch = 1 }\n",
        "artifact_visibility",
        &["field", "default"],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\n[layer.members]\nnonesuch = 1\n",
        "[layer.members]",
        &["source", "fields"],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\n[[layer.levels]]\nnonesuch = 1\n",
        "[[layer.levels]]",
        &["level", "title", "zoom"],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\n[layer.content]\nnonesuch = 1\n",
        "[layer.content]",
        &["computed", "supplied", "withdraw_on_member_deletion"],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\n[[layer.content.supplied]]\nnonesuch = 1\n",
        "[[layer.content.supplied]]",
        &["name", "type", "require_member_visibility"],
    );
}

/// Every key the two-file surface carried, refused by the unknown-field rule rather than aliased.
///
/// **Refusal is what tells a caller their file is stale** (decision 0048: replaced, not carried).
/// An alias would read a `listing = "public"` into a vocabulary's visibility silently, and a
/// `visible_when` into a membership requirement — each a disclosure control landing somewhere its
/// author did not write it.
#[test]
fn every_retired_key_is_refused_rather_than_aliased() {
    for (block, line) in [
        ("[[attribute]]\nname = \"a\"\n", "width = \"u8\""),
        ("[[attribute]]\nname = \"a\"\n", "listing = \"public\""),
        ("[[attribute]]\nname = \"a\"\n", "values_key = \"a\""),
        ("[[attribute]]\nname = \"a\"\n", "values_of = \"b\""),
        ("[[attribute]]\nname = \"a\"\n", "value_set = \"closed\""),
        ("[[attribute]]\nname = \"a\"\n", "values = [\"x\"]"),
        ("[[attribute]]\nname = \"a\"\n", "visibility = \"public\""),
        ("[[layer]]\nname = \"l\"\n", "gate = \"ir:analyst\""),
        ("[[layer]]\nname = \"l\"\n", "ungated = true"),
        ("[[layer]]\nname = \"l\"\n", "artifacts_carry_own = false"),
        ("[[layer]]\nname = \"l\"\n", "visible_when = \"none\""),
        ("[[layer]]\nname = \"l\"\n", "listing = \"public\""),
        ("[[vocabulary]]\nname = \"v\"\n", "listing = \"public\""),
        ("[[vocabulary]]\nname = \"v\"\n", "kind = \"declared\""),
    ] {
        let text = format!("{block}{line}\n");
        let message = err(&text);
        let key = line.split_whitespace().next().expect("a key");
        assert!(
            message.contains(key) && message.contains("unknown field"),
            "`{key}` must be refused as an unknown field, not aliased: {message}"
        );
    }
    // And `[layer.content]`'s own retired spelling.
    let text = "[[layer]]\nname = \"l\"\n[layer.content]\non_member_deletion = \"withdraw_content\"\n";
    assert!(err(text).contains("on_member_deletion"), "{}", err(text));
    // `derived` was the computed list's name; the retired word is refused where the new one lives.
    let text = "[[layer]]\nname = \"l\"\n[layer.content]\nderived = [\"centroid\"]\n";
    assert!(err(text).contains("derived"), "{}", err(text));
}

/// A mistyped disclosure control must not read as an absent one.
#[test]
fn an_unknown_key_is_refused_rather_than_ignored() {
    let text = SEVERITY.replace("visibility =", "visiblity =");
    let message = err(&text);
    assert!(
        message.contains("visiblity") || message.contains("unknown"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------------------------
// Vocabularies
// ---------------------------------------------------------------------------------------------

#[test]
fn a_closed_vocabulary_compiles_to_a_width_and_a_pinned_code_set() {
    let config = parse_str(SEVERITY).unwrap();
    assert_eq!(config.schema.attributes.len(), 1);
    assert_eq!(config.schema.attributes[0].ty, ScalarType::U8);
    assert_eq!(config.schema.row_bits(), Some(8));
    let vocab = &config.schema.vocabularies["severity"];
    assert_eq!(vocab.code_of("low"), Some(1));
    assert_eq!(vocab.code_of("nonesuch"), None);
    assert_eq!(vocab.visibility, Listing::Public);
    assert_eq!(vocab.value_set, ValueSet::Closed);
    assert_eq!(vocab.width, ScalarType::U8);
    assert_eq!(vocab.title.as_deref(), Some("Severity"));
}

/// **A bare key list assigns codes, and the assignment is recorded exactly as a pin is.** A caller
/// who does not care which integer a value gets should not have to invent one.
#[test]
fn a_bare_key_list_assigns_codes_in_the_order_given() {
    let text = SEVERITY.replace(
        "  [vocabulary.values]\n  low = 1\n  high = 2\n",
        "values     = [\"low\", \"medium\", \"high\"]\n",
    );
    let config = parse_str(&text).expect("a bare key list is a legal value set");
    let vocab = &config.schema.vocabularies["severity"];
    assert_eq!(vocab.code_of("low"), Some(1));
    assert_eq!(vocab.code_of("medium"), Some(2));
    assert_eq!(vocab.code_of("high"), Some(3));
    // Code 0 is the *absent* sentinel and is never assigned, which is why the first value is 1.
    assert!(!vocab.codes.values().any(|&c| c == ABSENT_CODE));
}

/// Assignment steps over the codes a caller already spent — pinned or retired.
#[test]
fn assignment_skips_pinned_and_reserved_codes() {
    let text = SEVERITY.replace(
        "  [vocabulary.values]\n  low = 1\n  high = 2\n",
        "values     = [\"low\", \"medium\", \"high\"]\nreserved   = [2]\n",
    );
    let config = parse_str(&text).unwrap();
    let vocab = &config.schema.vocabularies["severity"];
    assert_eq!(vocab.code_of("low"), Some(1));
    assert_eq!(vocab.code_of("medium"), Some(3), "2 is retired");
    assert_eq!(vocab.code_of("high"), Some(4));
}

#[test]
fn code_zero_is_refused_because_it_is_the_absent_sentinel() {
    let text = SEVERITY.replace("low = 1", "low = 0");
    assert!(err(&text).contains("absent"), "{}", err(&text));
}

#[test]
fn a_code_past_the_declared_width_is_refused_rather_than_widened() {
    let text = SEVERITY.replace("high = 2", "high = 300");
    let message = err(&text);
    assert!(message.contains("maximum of 255"), "{message}");
    assert!(message.contains("rebuild"), "{message}");
}

#[test]
fn two_values_at_one_code_are_refused() {
    let text = SEVERITY.replace("high = 2", "high = 1");
    assert!(err(&text).contains("share code 1"));
}

/// `reserved` is a tombstone, and reassigning a retired code silently recolours every row that
/// carried it.
#[test]
fn a_reserved_code_may_not_be_reassigned() {
    let text = SEVERITY.replace("visibility = \"public\"", "visibility = \"public\"\nreserved = [2]");
    assert!(err(&text).contains("reserved"), "{}", err(&text));
}

/// The three that must not default on a vocabulary, each for its own reason.
#[test]
fn width_value_set_and_visibility_are_each_required_on_a_vocabulary() {
    for (line, expected) in [
        ("width      = \"u8\"\n", "`width` is required"),
        ("value_set  = \"closed\"\n", "`value_set` is required"),
        ("visibility = \"public\"\n", "`visibility` is required"),
    ] {
        let text = SEVERITY.replace(line, "");
        let message = err(&text);
        assert!(message.contains(expected), "{message}");
        // The message must teach, not merely refuse: the values, spelled out.
        assert!(
            message.contains("no default"),
            "a required disclosure control must say why there is no default: {message}"
        );
    }
}

/// **A vocabulary's `visibility` takes no access label, and a word outside its two is never read as
/// one.** Reading it as a label would gate the value set on a term nobody holds — or, on the other
/// side of the typo, publish it.
#[test]
fn a_vocabularys_visibility_admits_exactly_two_words() {
    let derived = SEVERITY.replace("visibility = \"public\"", "visibility = \"derived\"");
    let config = parse_str(&derived).expect("`derived` is the other setting");
    assert_eq!(
        config.schema.vocabularies["severity"].visibility,
        Listing::PerViewer
    );

    for word in ["ir:analyst", "per_viewer", "inherited", "none", "Public"] {
        let text = SEVERITY.replace("visibility = \"public\"", &format!("visibility = \"{word}\""));
        let message = err(&text);
        assert!(
            message.contains("neither \"public\" nor \"derived\""),
            "`{word}` must be refused rather than read as a label: {message}"
        );
    }
}

/// A closed set is authored, and an authored set of nothing refuses every ingest.
#[test]
fn a_closed_vocabulary_with_no_value_source_is_refused() {
    let text = SEVERITY.replace("  [vocabulary.values]\n  low = 1\n  high = 2\n", "");
    let message = err(&text);
    assert!(message.contains("no value source"), "{message}");
    assert!(
        message.contains("never a silent fall-through to minting"),
        "the refusal must say what accepting it would have to do instead: {message}"
    );
}

/// An open vocabulary with no values is legal and starts empty — the build mints its first code
/// once the corpus supplies a key.
#[test]
fn an_open_vocabulary_with_no_values_starts_empty() {
    let text = SEVERITY
        .replace("  [vocabulary.values]\n  low = 1\n  high = 2\n", "")
        .replace("value_set  = \"closed\"", "value_set  = \"open\"")
        .replace("visibility = \"public\"", "visibility = \"derived\"");
    let config = parse_str(&text).unwrap();
    let vocab = &config.schema.vocabularies["severity"];
    assert_eq!(vocab.value_set, ValueSet::Open);
    assert!(vocab.codes.is_empty());
    assert_eq!(config.schema.open_minters().len(), 1);
}

/// A closed vocabulary mints nothing, so it must have no minter for a scan to reach.
#[test]
fn a_closed_vocabulary_has_no_minter() {
    assert!(parse_str(SEVERITY).unwrap().schema.open_minters().is_empty());
}

/// **Two vocabulary blocks of one name.** Two code spaces read back under one name, and which one
/// a stored code meant would depend on parse order.
#[test]
fn two_vocabularies_of_one_name_are_refused() {
    let text = format!(
        "{SEVERITY}\n[[vocabulary]]\nname = \"severity\"\nwidth = \"u16\"\nvalue_set = \"open\"\n\
         visibility = \"derived\"\n"
    );
    let message = err(&text);
    assert!(message.contains("declared twice"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------------------------

/// **An attribute naming an undeclared vocabulary is refused at config parse, before any data file
/// is opened** — and never minted as an implicit open vocabulary, which is the fall-through §7
/// forbids arriving through a typo.
#[test]
fn an_attribute_naming_an_undeclared_vocabulary_is_refused_at_parse() {
    let text = SEVERITY.replace("vocabulary = \"severity\"", "vocabulary = \"severtiy\"");
    let message = err(&text);
    assert!(message.contains("severtiy"), "{message}");
    assert!(
        message.contains("names no `[[vocabulary]]` block"),
        "{message}"
    );
    assert!(
        message.contains("Declared: severity"),
        "the refusal must name what is available: {message}"
    );
    assert!(
        message.contains("refused rather than minted"),
        "the refusal must say why a fall-through is not the answer: {message}"
    );
}

#[test]
fn a_category_needs_a_vocabulary_reference() {
    let text = SEVERITY.replace("vocabulary = \"severity\"\n", "");
    let message = err(&text);
    assert!(message.contains("`vocabulary` is required"), "{message}");
}

/// A category may declare `index` beside `render`, and it reaches the compiled form.
#[test]
fn a_category_may_declare_index() {
    let text = SEVERITY.replace("render     = true", "render     = true\nindex      = true");
    let config = parse_str(&text).expect("index is built for a category");
    assert!(config.schema.attributes[0].index);

    let render_only = parse_str(SEVERITY).expect("render alone stays legal");
    assert!(!render_only.schema.attributes[0].index);
}

/// `index` alone is accepted on every declarable type — a numeric is a value column and nothing
/// else, so there is no structure left for it to be waiting on.
#[test]
fn index_is_accepted_on_every_declarable_type() {
    for ty in [
        "bool",
        "u8",
        "u16",
        "u32",
        "u64",
        "i8",
        "i16",
        "i32",
        "i64",
        "f32",
        "f64",
        "timestamp_us",
    ] {
        let text = format!("[[attribute]]\nname = \"measure\"\ntype = \"{ty}\"\nindex = true\n");
        let config = parse_str(&text).unwrap_or_else(|e| panic!("{ty} must filter: {e}"));
        assert!(config.schema.attributes[0].index, "{ty}");
        assert!(!config.schema.attributes[0].render, "{ty}");
    }
}

/// A number, a datetime and a bool may be rendered **and** indexed — the two homes of one column,
/// which is what gives decision 0068 two routes to choose between on cost.
///
/// This combination was refused while 0064's render half was unbuilt, because the row route would
/// have read absence out of a hot column that stores it as the type's zero. The presence bitmap
/// beside the column is what removes that, and the row scan honours it (`viewport.rs`'s
/// `an_absent_number_matches_no_range_not_even_one_containing_zero`).
#[test]
fn a_number_may_be_rendered_and_indexed_at_once() {
    for ty in ["bool", "i32", "f64", "timestamp_us"] {
        let text =
            format!("[[attribute]]\nname = \"measure\"\ntype = \"{ty}\"\nrender = true\nindex = true\n");
        let config =
            parse_str(&text).unwrap_or_else(|e| panic!("{ty} must render and filter: {e}"));
        assert!(config.schema.attributes[0].index, "{ty}");
        assert!(config.schema.attributes[0].render, "{ty}");
    }
}

/// Decision 0013: absent machinery names itself rather than refusing generically. Bare `multi`
/// names records §5 and the epic that lifts it; `render` + `multi` names decision 0039's permanent
/// fence instead, whatever else the declaration says.
#[test]
fn multi_is_refused_naming_what_is_absent_and_0039_when_rendered() {
    let multi = SEVERITY.replace("render     = true", "index      = true\nmulti      = true");
    assert!(err(&multi).contains("records §5"), "{}", err(&multi));

    let rendered = SEVERITY.replace("render     = true", "render     = true\nmulti      = true");
    assert!(err(&rendered).contains("0039"), "{}", err(&rendered));
}

/// **`render_in` is refused rather than recorded and ignored** (decision 0013).
#[test]
fn render_in_is_refused_rather_than_silently_ignored() {
    let text = with_line(SEVERITY, "render_in = [\"docs_2024\"]");
    let message = err(&text);
    assert!(message.contains("§3.9"), "{message}");
    assert!(
        message.contains("every view anyway"),
        "the refusal must say what accepting it would actually do: {message}"
    );
    assert!(parse_str(SEVERITY).is_ok());
}

/// A declaration with neither `render` nor `index` parses and is blob-resident (records §3).
#[test]
fn a_declaration_with_neither_key_is_blob_resident() {
    let text = format!(
        "{SEVERITY}\n[[attribute]]\nname = \"notes\"\ntype = \"keyword\"\n\n\
         [[attribute]]\nname = \"revision\"\ntype = \"i64\"\n"
    );
    let config = parse_str(&text).expect("neither key declares a blob-resident column");
    for i in [1, 2] {
        assert!(!config.schema.attributes[i].index);
        assert!(!config.schema.attributes[i].render);
    }
    // The hot column's tail is exactly the render columns, so the blob-resident `i64` and the
    // entity-space `keyword` cost no row bits — only the rendered `u8` counts.
    assert_eq!(config.schema.row_bits(), Some(8));
}

#[test]
fn render_on_a_keyword_is_refused_at_the_declaration() {
    let message = err("[[attribute]]\nname = \"title\"\ntype = \"keyword\"\nrender = true\n");
    assert!(message.contains("fixed-width slot"), "{message}");
    assert!(
        message.contains("never leaves the server"),
        "a keyword's refusal must name the ordinal's confinement, not only the width: {message}"
    );
}

#[test]
fn render_on_text_is_refused_at_the_declaration() {
    let message = err("[[attribute]]\nname = \"abstract\"\ntype = \"text\"\nrender = true\n");
    assert!(message.contains("record blob"), "{message}");
}

#[test]
fn utf8_is_refused_as_a_declared_type_and_names_its_successors() {
    let message = err("[[attribute]]\nname = \"title\"\ntype = \"utf8\"\nindex = true\n");
    assert!(message.contains("retired"), "{message}");
    assert!(message.contains("keyword"), "{message}");
    assert!(message.contains("text"), "{message}");
}

#[test]
fn a_text_column_resolves_its_analyser_and_refuses_an_unknown_one() {
    let config =
        parse_str("[[attribute]]\nname = \"abstract\"\ntype = \"text\"\nindex = true\n").unwrap();
    assert_eq!(
        config.schema.attributes[0].analyser.as_deref(),
        Some("unicode/icu4x-2.2/p1"),
        "the default resolves to a full identity, not to a bare name"
    );

    let message = err(
        "[[attribute]]\nname = \"abstract\"\ntype = \"text\"\nanalyser = \"standard\"\n",
    );
    assert!(
        message.contains("standard") && message.contains("unicode"),
        "the refusal must name what was asked for and what is available: {message}"
    );
}

/// An analyser on a column that has no analyser is refused rather than ignored — the same rule a
/// vocabulary on a non-category gets, and for the same reason.
#[test]
fn an_analyser_on_a_non_text_column_is_refused() {
    let message = err("[[attribute]]\nname = \"score\"\ntype = \"i64\"\nanalyser = \"unicode\"\n");
    assert!(
        message.contains("not `text`") && message.contains("believes"),
        "{message}"
    );
    let message = err(&with_line(SEVERITY, "analyser = \"unicode\""));
    assert!(message.contains("not `text`"), "{message}");
}

/// A vocabulary on a non-category is a value set its author believes is in effect.
#[test]
fn a_vocabulary_on_a_non_category_is_refused_rather_than_ignored() {
    let text = format!(
        "{SEVERITY}\n[[attribute]]\nname = \"score\"\ntype = \"f32\"\nvocabulary = \"severity\"\n"
    );
    assert!(err(&text).contains("has no meaning"), "{}", err(&text));
}

#[test]
fn a_column_may_not_be_named_after_a_combinator() {
    for name in ["all_of", "any_of", "none_of"] {
        let text = format!("[[attribute]]\nname = \"{name}\"\ntype = \"keyword\"\nindex = true\n");
        assert!(err(&text).contains("filter combinator"), "{}", err(&text));
    }
}

#[test]
fn a_column_may_not_shadow_a_fixed_or_reserved_name() {
    for name in ["tessera_id", "residual", "x", "access"] {
        let text = format!("[[attribute]]\nname = \"{name}\"\ntype = \"i64\"\n");
        assert!(err(&text).contains("shadows"), "{name}: {}", err(&text));
    }
}

#[test]
fn record_is_a_reserved_column_name() {
    let message = err("[[attribute]]\nname = \"record\"\ntype = \"i64\"\n");
    assert!(message.contains("record blob"), "{message}");
}

#[test]
fn one_attribute_name_may_not_be_declared_twice() {
    let text = format!(
        "{SEVERITY}\n[[attribute]]\nname = \"severity\"\ntype = \"category\"\nvocabulary = \"severity\"\n"
    );
    assert!(err(&text).contains("declared twice"), "{}", err(&text));
}

/// The name addresses the column in `/v1/categories/{column}`, so it must survive a path segment.
#[test]
fn a_column_name_must_survive_a_path_segment() {
    for name in ["a/b", "a b", "a.b", "a%2Fb", "caté"] {
        let text = format!("[[attribute]]\nname = \"{name}\"\ntype = \"i64\"\n");
        assert!(err(&text).contains("its identifier on the wire"), "{name}");
    }
    for name in ["severity_2", "severity-2", "Severity2"] {
        let text = format!("[[attribute]]\nname = \"{name}\"\ntype = \"i64\"\n");
        assert!(parse_str(&text).is_ok(), "{name}");
    }
}

#[test]
fn an_attribute_needs_a_type() {
    let message = err("[[attribute]]\nname = \"score\"\n");
    assert!(message.contains("`type` is required"), "{message}");
    assert!(message.contains("timestamp_us"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

#[test]
fn a_view_compiles_its_point_visibility() {
    let config = parse_str(SEVERITY).unwrap();
    assert_eq!(config.views.len(), 1);
    assert_eq!(config.views[0].name, "s0");
    assert_eq!(
        config.views[0].point_visibility.field.as_deref(),
        Some("categories")
    );
    assert_eq!(config.views[0].point_visibility.default, "public");
}

#[test]
fn a_view_must_declare_its_point_visibility() {
    let text = SEVERITY.replace(
        "point_visibility = { field = \"categories\", default = \"public\" }\n",
        "",
    );
    let message = err(&text);
    assert!(message.contains("`point_visibility` is required"), "{message}");
    assert!(message.contains("no default"), "{message}");

    let text = SEVERITY.replace(
        "point_visibility = { field = \"categories\", default = \"public\" }",
        "point_visibility = { field = \"categories\" }",
    );
    assert!(
        err(&text).contains("`point_visibility.default` is required"),
        "{}",
        err(&text)
    );
}

/// **A point may not inherit.** A point carrying no terms is in no posting list and so in no
/// principal's mask, so inheriting would have to *add* a term — which can only widen it.
#[test]
fn a_point_default_may_not_be_inherited() {
    let text = SEVERITY.replace("default = \"public\"", "default = \"inherited\"");
    let message = err(&text);
    assert!(message.contains("refused"), "{message}");
    assert!(
        message.contains("can only widen"),
        "the refusal must say which direction it fails in: {message}"
    );
}

/// A view's own gate is specified and not implemented, so it is refused rather than recorded.
#[test]
fn a_views_own_visibility_is_refused_as_unbuilt() {
    let text = with_line(SEVERITY, "").replace(
        "name             = \"s0\"",
        "name             = \"s0\"\nvisibility       = \"ir:analyst\"",
    );
    let message = err(&text);
    assert!(message.contains("specified and not built"), "{message}");
    assert!(message.contains("views §3"), "{message}");
}

/// **The extent belongs to the view, not to the invocation** (§1). Four spellings, and each one
/// compiles to a frame or to the instruction to fit one.
#[test]
fn a_view_declares_its_quantisation_frame() {
    let frame = |declared: &str| {
        parse_str(&SEVERITY.replace("extent           = \"auto\"", declared))
            .unwrap_or_else(|e| panic!("{declared}: {e}"))
            .views[0]
            .extent
    };
    assert_eq!(
        frame("extent = \"auto\""),
        Extent::Auto {
            margin: DEFAULT_AUTO_MARGIN
        }
    );
    assert_eq!(
        frame("extent = { auto = true, margin = 0.25 }"),
        Extent::Auto { margin: 0.25 }
    );
    assert_eq!(
        frame("extent = { min = -25.0, max = 25.0 }"),
        Extent::Fixed(Bounds {
            x_min: -25.0,
            x_max: 25.0,
            y_min: -25.0,
            y_max: 25.0
        })
    );
    assert_eq!(
        frame("extent = { x = [-18, 19], y = [-22, 24] }"),
        Extent::Fixed(Bounds {
            x_min: -18.0,
            x_max: 19.0,
            y_min: -22.0,
            y_max: 24.0
        })
    );
}

/// **`extent` has no default**, because the value an absent line would supply decides where every
/// stored point lands — and quantisation clamps, so a guessed frame is a bundle that is
/// well-formed with the geometry wrong.
#[test]
fn a_view_must_declare_its_extent() {
    let text = SEVERITY.replace("extent           = \"auto\"\n", "");
    let message = err(&text);
    assert!(message.contains("`extent` is required"), "{message}");
    assert!(message.contains("no default"), "{message}");
    // Every refusal carries all four spellings: the author is choosing a shape, not fixing a typo.
    for spelling in ["\"auto\"", "margin", "min = -25.0", "x = [-18, 19]"] {
        assert!(message.contains(spelling), "{spelling} missing from: {message}");
    }
}

/// The ways half an extent is written, each refused with what the halves mean.
#[test]
fn half_an_extent_is_refused_rather_than_completed() {
    let refused = |declared: &str| err(&SEVERITY.replace("extent           = \"auto\"", declared));

    let message = refused("extent = { min = -25.0 }");
    assert!(message.contains("half a frame"), "{message}");
    assert!(message.contains("min"), "{message}");

    let message = refused("extent = { x = [-18, 19] }");
    assert!(message.contains("half a frame"), "{message}");

    let message = refused("extent = { }");
    assert!(message.contains("empty table"), "{message}");

    // `margin` is `auto`'s parameter: a box stated outright already carries whatever headroom its
    // author wanted, so a margin beside one is a line that would do nothing.
    let message = refused("extent = { min = -25.0, max = 25.0, margin = 0.1 }");
    assert!(message.contains("without `auto = true`"), "{message}");

    // Fitting the box and stating it are alternatives, not a base and an override.
    let message = refused("extent = { auto = true, min = -25.0, max = 25.0 }");
    assert!(message.contains("`auto` and a stated box"), "{message}");

    let message = refused("extent = { auto = false }");
    assert!(message.contains("says what the frame is not"), "{message}");

    let message = refused("extent = \"whole\"");
    assert!(message.contains("not a value this key takes"), "{message}");

    // A degenerate box would make every division by the span a NaN, silently.
    let message = refused("extent = { min = 25.0, max = 25.0 }");
    assert!(message.contains("x_max > x_min"), "{message}");

    // A negative margin shrinks the box inside the data and clamps what it excluded.
    let message = refused("extent = { auto = true, margin = -0.1 }");
    assert!(message.contains("not a fraction of the data span"), "{message}");
}

#[test]
fn two_views_of_one_name_are_refused() {
    let text = format!(
        "{SEVERITY}\n[[view]]\nname = \"s0\"\nextent = \"auto\"\npoint_visibility = {{ default = \"public\" }}\n"
    );
    assert!(err(&text).contains("declared twice"), "{}", err(&text));
}

// ---------------------------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------------------------

#[test]
fn a_layer_compiles_its_two_axes() {
    let config = parse_str(&with_layer("")).expect("the fixture layer parses");
    let layer = &config.layers[0];
    assert_eq!(layer.name, "clusters/a");
    // `public` is the absence of a gate today, and a *term* once the dictionary carries one.
    assert_eq!(layer.visibility, None);
    assert!(!layer.artifact_visibility.carry_own());
    assert_eq!(layer.artifact_visibility.default, MemberDefault::Inherited);
    assert_eq!(
        layer.require_member_visibility,
        Some(ExistenceCriterion::Fraction(0.05))
    );
    assert!(layer.hierarchy.prune_children);
    assert_eq!(layer.content.computed, vec!["centroid", "box"]);
    // `[layer.content]`'s withdrawal default is the half that cannot widen.
    assert!(layer.content.withdraw_on_member_deletion);
}

/// The three §6 requires on every layer, each with its own reason for having no default.
#[test]
fn a_layer_declares_all_three_disclosure_controls() {
    for (line, expected) in [
        ("visibility                = \"public\"\n", "`visibility` is required"),
        (
            "artifact_visibility       = { default = \"inherited\" }\n",
            "`artifact_visibility` is required",
        ),
        (
            "require_member_visibility = { fraction = 0.05 }\n",
            "`require_member_visibility` is required",
        ),
    ] {
        let text = with_layer("").replace(line, "");
        let message = err(&text);
        assert!(message.contains(expected), "{message}");
        assert!(
            message.contains("no default"),
            "a required disclosure control must say why there is no default: {message}"
        );
    }
}

/// `require_member_visibility`'s five settings, and the two mechanisms behind them.
#[test]
fn the_membership_requirement_has_five_settings() {
    for (written, expected) in [
        ("\"none\"", None),
        ("\"any\"", Some(ExistenceCriterion::Count(1))),
        ("\"all\"", Some(ExistenceCriterion::Fraction(1.0))),
        ("{ count = 1000 }", Some(ExistenceCriterion::Count(1000))),
        ("{ fraction = 0.1 }", Some(ExistenceCriterion::Fraction(0.1))),
    ] {
        let text = with_layer("").replace(
            "require_member_visibility = { fraction = 0.05 }",
            &format!("require_member_visibility = {written}"),
        );
        let config = parse_str(&text).unwrap_or_else(|e| panic!("{written}: {e}"));
        assert_eq!(config.layers[0].require_member_visibility, expected, "{written}");
    }
    let text = with_layer("").replace(
        "require_member_visibility = { fraction = 0.05 }",
        "require_member_visibility = \"some\"",
    );
    assert!(err(&text).contains("none of"), "{}", err(&text));
    let text = with_layer("").replace(
        "require_member_visibility = { fraction = 0.05 }",
        "require_member_visibility = { count = 1, fraction = 0.5 }",
    );
    assert!(err(&text).contains("exactly one of"), "{}", err(&text));
}

/// **An empty closed vocabulary is refused in every spelling**, the rule applying after the three
/// converge rather than at the source.
///
/// Refusing only *the absence of a source* would admit a source that declares nothing — the same
/// column, the same width in every row, and none of the message. A closed set is the authority on
/// what may be ingested, so an empty one refuses every value for ever.
#[test]
fn a_closed_vocabulary_with_no_values_is_refused_in_every_spelling() {
    // Both spellings of "authored, and authoring nothing": the inline table emptied, and the bare
    // key array emptied. `SEVERITY` pins two codes, so removing them is the whole edit.
    for emptied in ["  [vocabulary.values]\n", "values = []\n"] {
        let text = SEVERITY
            .replace("  [vocabulary.values]\n  low = 1\n  high = 2\n", emptied)
            .to_string();
        let message = err(&text);
        assert!(
            message.contains("no values") || message.contains("value source"),
            "{emptied:?}: {message}"
        );
        assert!(
            message.contains("open"),
            "{emptied:?}: the refusal must name the other value_set: {message}"
        );
    }
    // An open one is legal empty: its values arrive as they are minted.
    let text = SEVERITY
        .replace("  [vocabulary.values]\n  low = 1\n  high = 2\n", "values = []\n")
        .replace("value_set  = \"closed\"", "value_set  = \"open\"");
    parse_str(&text).expect("an open vocabulary may start empty");
}

/// A threshold that cannot fail, or cannot pass, is refused rather than compiled.
///
/// `{ count = 0 }` clears on every masked count and `{ fraction = 0.0 }` with it, so each declares
/// a requirement and imposes none — the shape decision 0084 rules out by making an *absent*
/// criterion its own declaration. A share above one is the mirror: nothing can ever clear it, so it
/// hides the layer while reading as a threshold. The caller who means *no rule* has a word.
#[test]
fn a_threshold_that_cannot_fail_or_cannot_pass_is_refused() {
    for (written, expected) in [
        ("{ count = 0 }", "at least 1"),
        ("{ fraction = 0.0 }", "in (0, 1]"),
        ("{ fraction = 1.5 }", "in (0, 1]"),
        ("{ fraction = -0.5 }", "in (0, 1]"),
    ] {
        let text = with_layer("").replace(
            "require_member_visibility = { fraction = 0.05 }",
            &format!("require_member_visibility = {written}"),
        );
        let message = err(&text);
        assert!(message.contains(expected), "{written}: {message}");
        assert!(
            message.contains("\"none\""),
            "{written}: the refusal must name the word that means no rule: {message}"
        );
    }
    // The bounds themselves stand: one member, and the whole membership.
    for written in ["{ count = 1 }", "{ fraction = 1.0 }"] {
        let text = with_layer("").replace(
            "require_member_visibility = { fraction = 0.05 }",
            &format!("require_member_visibility = {written}"),
        );
        parse_str(&text).unwrap_or_else(|e| panic!("{written} is legal: {e}"));
    }
}

/// **An access label spelled `inherited` is refused**, at every slot that takes one — it is the one
/// reserved word occupying such a slot, so a layer gated on a real term of that name and one
/// saying *the container's gate is the whole of it* would be the same eight characters.
#[test]
fn an_access_label_may_not_be_spelled_inherited() {
    let text = with_layer("").replace("visibility                = \"public\"", "visibility                = \"inherited\"");
    let message = err(&text);
    assert!(message.contains("may not be spelled `inherited`"), "{message}");
    assert!(
        message.contains("`public` is not reserved in this sense"),
        "the refusal must say why the other reserved word is fine: {message}"
    );

    // ...and it is legal where it is not a label: the member default.
    let text = with_layer("").replace(
        "artifact_visibility       = { default = \"inherited\" }",
        "artifact_visibility       = { field = \"visibility\", default = \"ir:analyst\" }",
    );
    let config = parse_str(&text).expect("a member default may be a label");
    assert!(config.layers[0].artifact_visibility.carry_own());
    assert_eq!(
        config.layers[0].artifact_visibility.default,
        MemberDefault::Label("ir:analyst".into())
    );
}

/// **`public` is a label, not a reserved absence**, so it is accepted wherever a label goes.
#[test]
fn public_is_a_label_and_is_never_refused() {
    let text = with_layer("").replace(
        "artifact_visibility       = { default = \"inherited\" }",
        "artifact_visibility       = { field = \"visibility\", default = \"public\" }",
    );
    let config = parse_str(&text).expect("`public` is a label");
    assert_eq!(
        config.layers[0].artifact_visibility.default,
        MemberDefault::Label("public".into())
    );
}

/// **A layer naming an undeclared view is refused at parse.** A layer in a view that does not
/// exist is registered, reachable and empty, which no client can tell from one whose artifacts
/// were all withheld.
#[test]
fn a_layer_naming_an_undeclared_view_is_refused() {
    let text = with_layer("").replace("views                     = [\"s0\"]", "views                     = [\"s7\"]");
    let message = err(&text);
    assert!(message.contains("s7"), "{message}");
    assert!(message.contains("Declared: s0"), "{message}");
}

#[test]
fn a_layer_declares_its_hierarchy_rather_than_having_it_inferred() {
    let text = with_layer("").replace("hierarchy                 = { kind = \"flat\", prune_children = true }\n", "");
    let message = err(&text);
    assert!(message.contains("`hierarchy` is required"), "{message}");
    assert!(message.contains("tiered"), "the four kinds must be named: {message}");

    let text = with_layer("").replace("kind = \"flat\"", "kind = \"treed\"");
    assert!(err(&text).contains("none of"), "{}", err(&text));
}

/// The levels rule follows from the kind, and the check is the registry's own — one implementation
/// of the rules, fired at parse rather than part-way through a build.
#[test]
fn the_levels_rule_follows_the_declared_kind() {
    let levels = "\n[[layer.levels]]\nlevel = 0\ntitle = \"countries\"\n";
    let nested = with_layer("").replace("kind = \"flat\"", "kind = \"nested\"");
    assert!(parse_str(&nested).is_ok(), "a nested layer declares no levels");
    assert!(
        err(&format!("{nested}{levels}")).contains("declares no levels"),
        "{}",
        err(&format!("{nested}{levels}"))
    );

    for kind in ["stacked", "tiered"] {
        let text = with_layer("").replace("kind = \"flat\"", &format!("kind = \"{kind}\""));
        // A predicate-free enumerated layer with a proportional criterion is fine; the levels are
        // what these two need.
        assert!(
            err(&text).contains("must declare"),
            "{kind}: {}",
            err(&text)
        );
        assert!(parse_str(&format!("{text}{levels}")).is_ok(), "{kind}");
    }
}

#[test]
fn a_level_declares_its_number_and_its_title() {
    let base = with_layer("").replace("kind = \"flat\"", "kind = \"stacked\"");
    let message = err(&format!("{base}\n[[layer.levels]]\ntitle = \"countries\"\n"));
    assert!(message.contains("declares no `level` number"), "{message}");
    // A title is presentation metadata and optional everywhere in the surface: it discloses
    // nothing a name does not, and the name is already served, so absent is served as absent
    // rather than as an identity the service decided to display.
    let config = parse_str(&format!("{base}\n[[layer.levels]]\nlevel = 0\n"))
        .expect("a level need not be titled");
    assert_eq!(config.layers[0].levels[0].title, None);
    // Levels address by `entity − base`, so a gap reserves a run nothing reaches.
    let message = err(&format!(
        "{base}\n[[layer.levels]]\nlevel = 0\ntitle = \"a\"\n\n[[layer.levels]]\nlevel = 2\ntitle = \"c\"\n"
    ));
    assert!(message.contains("0..n"), "{message}");
}

/// **Supplied content declares its own membership requirement, and it has no default** (C28).
#[test]
fn supplied_content_declares_its_membership_requirement() {
    let supplied =
        "\n[[layer.content.supplied]]\nname = \"topic\"\ntype = \"text\"\nrequire_member_visibility = \"all\"\n";
    let config = parse_str(&with_layer(supplied)).expect("supplied content parses");
    let content = &config.layers[0].content.supplied[0];
    assert_eq!(content.name, "topic");
    assert_eq!(content.ty, "text");
    assert!(content.require_member_visibility.is_corpus_derived());

    let without =
        "\n[[layer.content.supplied]]\nname = \"topic\"\ntype = \"text\"\n";
    let message = err(&with_layer(without));
    assert!(message.contains("no `require_member_visibility`"), "{message}");
    assert!(message.contains("C28"), "{message}");

    let no_type = "\n[[layer.content.supplied]]\nname = \"topic\"\nrequire_member_visibility = \"all\"\n";
    assert!(
        err(&with_layer(no_type)).contains("declares no `type`"),
        "{}",
        err(&with_layer(no_type))
    );

    let bad_type = "\n[[layer.content.supplied]]\nname = \"topic\"\ntype = \"label_text\"\nrequire_member_visibility = \"all\"\n";
    assert!(
        err(&with_layer(bad_type)).contains("none of text, polygon, extent or point"),
        "{}",
        err(&with_layer(bad_type))
    );

    let bad_requirement = "\n[[layer.content.supplied]]\nname = \"topic\"\ntype = \"text\"\nrequire_member_visibility = \"any\"\n";
    assert!(
        err(&with_layer(bad_requirement)).contains("only \"all\" or \"inherited\""),
        "{}",
        err(&with_layer(bad_requirement))
    );
}

/// **`withdraw_on_member_deletion` on a *layer* is specified and not implemented**, so `true` is
/// refused rather than accepted and quietly ignored: the fold has no artifact-withdrawal path, and
/// a declaration that does nothing is exactly the hazard this surface exists to prevent.
#[test]
fn withdrawing_a_whole_artifact_is_refused_as_unbuilt() {
    let text = with_layer("").replace(
        "membership                = \"enumerated\"",
        "membership                = \"enumerated\"\nwithdraw_on_member_deletion = true",
    );
    let message = err(&text);
    assert!(message.contains("specified and not built"), "{message}");
    assert!(message.contains("annotation-write-cycle.md §6.1"), "{message}");
    assert!(
        message.contains("`false` is the default"),
        "the refusal must say what happens instead: {message}"
    );

    // `false` is the declaration of the current behaviour, and is accepted.
    let text = with_layer("").replace(
        "membership                = \"enumerated\"",
        "membership                = \"enumerated\"\nwithdraw_on_member_deletion = false",
    );
    assert!(parse_str(&text).is_ok());
}

/// The **content**-level key of the same name is built, defaults `true`, and `false` is the
/// widening half a caller must type.
#[test]
fn content_withdrawal_defaults_to_the_half_that_cannot_widen() {
    let text = with_layer("").replace(
        "  computed = [\"centroid\", \"box\"]",
        "  computed = [\"centroid\"]\n  withdraw_on_member_deletion = false",
    );
    let config = parse_str(&text).unwrap();
    assert!(!config.layers[0].content.withdraw_on_member_deletion);
    assert!(parse_str(&with_layer(""))
        .unwrap()
        .layers[0]
        .content
        .withdraw_on_member_deletion);
}

#[test]
fn a_layer_declares_its_membership_source() {
    let text = with_layer("").replace("membership                = \"enumerated\"\n", "");
    assert!(err(&text).contains("`membership` is required"), "{}", err(&text));
    let text = with_layer("").replace("\"enumerated\"", "\"predicate\"");
    assert!(err(&text).contains("none of"), "{}", err(&text));
}

/// A computed property outside the closed vocabulary is refused rather than ignored: an artifact
/// served without content its layer declared cannot be told apart from one whose content was
/// withheld.
#[test]
fn an_unknown_computed_property_is_refused() {
    let text = with_layer("").replace("[\"centroid\", \"box\"]", "[\"hulls\"]");
    assert!(err(&text).contains("hulls"), "{}", err(&text));
}

// ---------------------------------------------------------------------------------------------
// Acquisition: sources, fields and the binding
// ---------------------------------------------------------------------------------------------

/// `SEVERITY`'s corpus with every source it can carry declared, for the acquisition cases.
///
/// Every path is **relative to the declaring document** (`configuration.md` §3), which is what
/// lets one file describe a corpus on a laptop and in CI without an invocation naming five files.
const ACQUIRED: &str = r#"
[corpus]
source = "corpus.parquet"

[[view]]
name             = "s0"
extent           = "auto"
source           = "geometry.parquet"
point_visibility = { source = "pairs.parquet", default = "public" }

[[vocabulary]]
name       = "severity"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = ["low", "high"]

[[attribute]]
name       = "severity"
type       = "category"
vocabulary = "severity"
"#;

/// `--file KEY=PATH`, as the CLI hands it over: an override keyed by the **object** whose source
/// it replaces. The paths need not exist — every case here is refused, or answered, before a data
/// file is opened.
fn files(keys: &[&str]) -> HashMap<String, PathBuf> {
    keys.iter()
        .map(|key| {
            (
                (*key).to_string(),
                PathBuf::from(format!("/elsewhere/{}.parquet", key.replace(':', "-"))),
            )
        })
        .collect()
}

fn bound_err(text: &str, keys: &[&str]) -> String {
    format!(
        "{}",
        parse_bound(text, &files(keys)).expect_err("expected a refusal")
    )
}

/// **Every source is a path relative to the document that declares it** (§3). No binding, no
/// invocation: the config alone says where its corpus is, and it says so the same way from any
/// working directory.
#[test]
fn a_source_is_a_path_relative_to_the_declaring_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = parse_at(dir.path(), ACQUIRED, &HashMap::new())
        .expect("a declaration naming its own files needs no bindings");
    assert_eq!(config.corpus.source, Some(dir.path().join("corpus.parquet")));
    assert_eq!(config.views[0].source, Some(dir.path().join("geometry.parquet")));
    assert_eq!(
        config.views[0].point_visibility.source,
        Some(dir.path().join("pairs.parquet"))
    );
}

/// **An absolute source is refused**, which is the rule §4 was always protecting: a path that
/// cannot travel with the document describes a corpus on one machine, and the next reader of the
/// repository gets a file-not-found rather than a declaration they can act on.
#[test]
fn an_absolute_source_is_refused_and_names_the_override() {
    let text = ACQUIRED.replace("source           = \"geometry.parquet\"", "source           = \"/mnt/scratch/geometry.parquet\"");
    let message = bound_err(&text, &[]);
    assert!(message.contains("is an absolute path"), "{message}");
    assert!(message.contains("relative to this config"), "{message}");
    assert!(
        message.contains("--file view:s0=/mnt/scratch/geometry.parquet"),
        "the refusal must name where an absolute path does belong: {message}"
    );
}

/// `--file` **overrides one source**, keyed by the object whose source it is.
#[test]
fn an_override_replaces_one_objects_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = parse_at(dir.path(), ACQUIRED, &files(&["view:s0"]))
        .expect("an override of a declared source");
    assert_eq!(
        config.views[0].source,
        Some(PathBuf::from("/elsewhere/view-s0.parquet"))
    );
    // Everything it did not name is still the declaration's own path.
    assert_eq!(config.corpus.source, Some(dir.path().join("corpus.parquet")));
}

/// **An override that names nothing is a refusal**, listing the keys that exist. Without this the
/// declaration's own path stays quietly in force under a command line asking for another corpus.
#[test]
fn an_override_no_object_declares_is_refused() {
    let message = bound_err(ACQUIRED, &["view:s9"]);
    assert!(message.contains("'view:s9=…'"), "{message}");
    assert!(message.contains("names no source in this config"), "{message}");
    assert!(
        message.contains("view:s0") && message.contains("corpus"),
        "the refusal must list the keys that exist: {message}"
    );
}

/// An override **never creates** a source, so a closed vocabulary cannot be opened from the
/// command line — the fall-through §8 forbids, arriving through an invocation instead of a typo.
#[test]
fn an_override_is_never_a_fall_through_to_minting() {
    // The vocabulary declares its values inline and names no source at all, so there is no
    // `vocabulary:severity` key to override.
    let message = bound_err(ACQUIRED, &["vocabulary:severity"]);
    assert!(message.contains("names no source in this config"), "{message}");
    assert!(
        message.contains("never creates one"),
        "the refusal must say why an override cannot stand alone: {message}"
    );
}

/// A value set is inline **or** sourced: two spellings of one thing, so both is a parse error
/// rather than a precedence question.
#[test]
fn inline_values_and_a_source_together_are_refused() {
    let text = ACQUIRED.replace(
        "values     = [\"low\", \"high\"]",
        "values     = [\"low\", \"high\"]\nsource     = \"severity.parquet\"",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("spellings of one thing"), "{message}");
}

/// **The map says *where*, never *whether*.** A name outside the object's fields is refused
/// rather than passed to the reader.
#[test]
fn a_field_map_may_not_name_a_field_the_object_does_not_have() {
    let text = ACQUIRED.replace(
        "source           = \"geometry.parquet\"",
        "source           = \"geometry.parquet\"\nfields           = { nonesuch = \"nonesuch\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("`fields.nonesuch`"), "{message}");
    assert!(message.contains("not one of this object's fields"), "{message}");
    assert!(message.contains("entity_id"), "the refusal must list them: {message}");
}

/// A field the object never declared is refused, and the refusal names the key that *would*
/// declare it — locating a field is not a way to assert one exists.
#[test]
fn a_field_map_may_not_name_a_field_the_object_never_declared() {
    // `parent` on a flat layer: the hierarchy kind is what says there are lineage edges.
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan.parquet\"\nfields                    = { parent = \"parent\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("never declared"), "{message}");
    assert!(message.contains("hierarchy.kind"), "{message}");

    // `attached_key` with no `depends_on`: an edge points into a layer this one never named.
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan.parquet\"\nfields                    = { attached_key = \"attached_key\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("depends_on"), "{message}");

    // `contents` with no supplied content declared.
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan.parquet\"\nfields                    = { contents = \"contents\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("layer.content.supplied"), "{message}");
}

/// A map with no source names the fields of nothing.
#[test]
fn a_field_map_without_a_source_is_refused() {
    let text = ACQUIRED.replace("source = \"corpus.parquet\"", "fields = { entity_id = \"entity_id\" }");
    let message = bound_err(&text, &[]);
    assert!(message.contains("`fields` without a `source`"), "{message}");
}

/// ⊘ A map may not yet *move* a field: the readers resolve the canonical names, so an entry that
/// renamed one would parse and do nothing — a column its author believes is being read.
#[test]
fn a_renamed_field_is_refused_rather_than_disregarded() {
    let text = ACQUIRED.replace(
        "source = \"corpus.parquet\"",
        "source = \"corpus.parquet\"\nfields = { entity_id = \"id\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("specified and not built"), "{message}");
    assert!(message.contains("entity_id"), "{message}");
    assert!(message.contains("'id'"), "{message}");

    // An attribute's own one-field map is the same rule.
    let text = with_line(SEVERITY, "field = \"sev\"");
    let message = err(&text);
    assert!(message.contains("specified and not built"), "{message}");
}

/// **A point's label comes from a field or from a source, never both** (§1).
#[test]
fn a_point_label_comes_from_a_field_or_a_source_never_both() {
    let text = ACQUIRED.replace(
        "point_visibility = { source = \"pairs.parquet\", default = \"public\" }",
        "point_visibility = { source = \"pairs.parquet\", field = \"categories\", default = \"public\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("both a `field` and a `source`"), "{message}");
}

/// The two geometry shapes are mutually exclusive: a row carries coordinates or a code.
#[test]
fn the_two_geometry_shapes_may_not_both_be_located() {
    let text = ACQUIRED.replace(
        "source           = \"geometry.parquet\"",
        "source           = \"geometry.parquet\"\nfields           = { x = \"x\", morton = \"morton\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("mutually exclusive"), "{message}");

    let text = ACQUIRED.replace(
        "source           = \"geometry.parquet\"",
        "source           = \"geometry.parquet\"\nfields           = { residual = \"residual\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("without `fields.morton`"), "{message}");
}

/// A layer's membership has two shapes and it names whichever it uses — never both.
#[test]
fn membership_is_a_list_field_or_a_source_never_both() {
    let text = with_layer("  [layer.members]\n  source = \"hdbscan_members.parquet\"\n").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan.parquet\"\nfields                    = { members = \"members\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("membership is declared twice"), "{message}");
}

/// A member source with no roster would make every mistyped key its own artifact.
#[test]
fn a_member_source_needs_the_layers_own_source() {
    let text = with_layer("  [layer.members]\n  source = \"hdbscan_members.parquet\"\n");
    let message = bound_err(&text, &[]);
    assert!(message.contains("roster"), "{message}");
}

/// ⊘ The label sugar and an inline artifact list are specified and not built, and each refusal
/// names which it is rather than reading as a typo.
#[test]
fn the_unbuilt_layer_blocks_name_what_is_absent() {
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nartifacts                 = []",
    );
    let message = err(&text);
    assert!(message.contains("specified and not built"), "{message}");
    assert!(message.contains("read from the file `source` names"), "{message}");

    let text = with_layer("  [layer.labels]\n  name = \"topics\"\n");
    let message = err(&text);
    assert!(message.contains("specified and not built"), "{message}");
    assert!(message.contains("label sugar"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// What one build reads: `Config::acquire`
// ---------------------------------------------------------------------------------------------

/// The files a build reads, resolved from the declaration and the bindings.
#[test]
fn acquisition_names_the_files_this_build_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = parse_at(dir.path(), ACQUIRED, &HashMap::new()).unwrap();
    let acquired = config.acquire("s0").expect("the view is declared");
    assert_eq!(acquired.points, dir.path().join("geometry.parquet"));
    assert_eq!(acquired.pairs, dir.path().join("pairs.parquet"));
    assert_eq!(acquired.corpus, Some(dir.path().join("corpus.parquet")));
    assert_eq!(acquired.artifacts, None);
    assert_eq!(acquired.artifact_members, None);
    assert_eq!(
        acquired.extent,
        Extent::Auto {
            margin: DEFAULT_AUTO_MARGIN
        }
    );
}

/// A layer's own source and its members', bound and picked up by the build.
#[test]
fn a_layer_names_its_artifacts_and_its_members() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = format!(
        "{ACQUIRED}{}\n  [layer.members]\n  source = \"hdbscan_members.parquet\"\n",
        LAYER.replace(
            "views                     = [\"s0\"]",
            "views                     = [\"s0\"]\nsource                    = \"hdbscan.parquet\"",
        )
    );
    let config = parse_at(dir.path(), &text, &HashMap::new())
        .expect("the layer's two sources are declared");
    let acquired = config.acquire("s0").unwrap();
    assert_eq!(acquired.artifacts, Some(dir.path().join("hdbscan.parquet")));
    assert_eq!(
        acquired.artifact_members,
        Some(dir.path().join("hdbscan_members.parquet"))
    );
    // And the members source is overridable on its own key, without disturbing the roster.
    let config = parse_at(
        dir.path(),
        &text,
        &files(&["layer:clusters/a:members"]),
    )
    .expect("one source staged elsewhere");
    let acquired = config.acquire("s0").unwrap();
    assert_eq!(acquired.artifacts, Some(dir.path().join("hdbscan.parquet")));
    assert_eq!(
        acquired.artifact_members,
        Some(PathBuf::from("/elsewhere/layer-clusters/a-members.parquet"))
    );
}

/// ⊘ One file per layer is not built: the artifact file carries a `layer` column and the build
/// reads one path, so two layers naming different keys would leave the second unread.
#[test]
fn two_layers_binding_different_artifact_files_are_refused() {
    let second = LAYER
        .replace("clusters/a", "clusters/b")
        .replace(
            "views                     = [\"s0\"]",
            "views                     = [\"s0\"]\nsource                    = \"other.parquet\"",
        );
    let first = LAYER.replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan.parquet\"",
    );
    let text = format!("{ACQUIRED}{first}{second}");
    let config = parse_bound(&text, &HashMap::new()).expect("both layers parse");
    let message = format!("{}", config.acquire("s0").expect_err("expected a refusal"));
    assert!(message.contains("specified and not built"), "{message}");
    assert!(message.contains("one file per layer"), "{message}");
}

/// The build materialises one view and reads its source, so a `--view` it cannot find is a build
/// with no geometry rather than a default one.
#[test]
fn a_build_refuses_a_view_the_config_does_not_declare() {
    let config = parse_bound(ACQUIRED, &HashMap::new()).unwrap();
    let message = format!("{}", config.acquire("s9").expect_err("expected a refusal"));
    assert!(message.contains("--view 's9'"), "{message}");
    assert!(message.contains("s0"), "the refusal must list them: {message}");
}

/// ⊘ A view declaring no source is legal and means the view is declared and empty — a bundle with
/// no rows in it, which is not built.
#[test]
fn a_build_refuses_a_view_with_no_source() {
    let text = ACQUIRED.replace("source           = \"geometry.parquet\"\n", "");
    let config = parse_bound(&text, &HashMap::new()).unwrap();
    let message = format!("{}", config.acquire("s0").expect_err("expected a refusal"));
    assert!(message.contains("`source` is required to build"), "{message}");
}

/// ⊘ Both label routes that are not built refuse rather than reading nothing: a corpus whose every
/// point carries no term sits in no principal's mask, and the bundle would come up empty for
/// everyone with no error anywhere.
#[test]
fn a_build_refuses_the_two_label_routes_that_are_not_built() {
    for (declared, expected) in [
        ("{ field = \"categories\", default = \"public\" }", "point_visibility.field"),
        ("{ default = \"public\" }", "only a `default`"),
    ] {
        let text = ACQUIRED.replace(
            "{ source = \"pairs.parquet\", default = \"public\" }",
            declared,
        );
        let config = parse_bound(&text, &HashMap::new()).unwrap();
        let message = format!("{}", config.acquire("s0").expect_err("expected a refusal"));
        assert!(message.contains("specified and not built"), "{message}");
        assert!(message.contains(expected), "{message}");
        assert!(
            message.contains("no principal's mask") || message.contains("reserved term"),
            "the refusal must say what reading nothing would produce: {message}"
        );
    }
}

/// Attributes with no corpus source: the pass has no file to read its columns from, and every
/// staged item must receive a value.
#[test]
fn a_build_refuses_attributes_with_no_corpus_source() {
    let text = ACQUIRED.replace("[corpus]\nsource = \"corpus.parquet\"\n", "");
    let config = parse_bound(&text, &HashMap::new()).unwrap();
    let message = format!("{}", config.acquire("s0").expect_err("expected a refusal"));
    assert!(message.contains("`[corpus]` names no `source`"), "{message}");
    assert!(message.contains("attribute(s) are declared"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// The whole document
// ---------------------------------------------------------------------------------------------

/// An empty config is legal and declares nothing — the state every bundle built before this input
/// existed was in, and the one a deployment that writes through the service starts from.
#[test]
fn an_empty_config_declares_nothing() {
    let config = parse_str("").expect("an empty config is legal");
    assert!(config.schema.is_empty());
    assert!(config.views.is_empty());
    assert!(config.layers.is_empty());
}

/// A layer name is tombstoned on drop and never reused, so two blocks of one name is not a
/// last-one-wins question: bookmarks, edges and suppressions all travel by it.
#[test]
fn two_layers_of_one_name_are_refused() {
    let text = with_layer(LAYER);
    assert!(err(&text).contains("declared twice"), "{}", err(&text));
}

/// A key listed twice in a bare array: which code it would take is decided by position.
#[test]
fn a_value_listed_twice_in_an_array_is_refused() {
    let text = SEVERITY.replace(
        "  [vocabulary.values]\n  low = 1\n  high = 2\n",
        "values     = [\"low\", \"high\", \"low\"]\n",
    );
    let message = err(&text);
    assert!(message.contains("listed twice"), "{message}");
}

/// Declaration order is registration order, and a layer must follow every layer it names.
#[test]
fn layers_keep_their_declaration_order() {
    let second = "\n[[layer]]\nname                      = \"topics/x\"\ntitle                     = \"topics\"\n\
                  views                     = [\"s0\"]\nmembership                = \"enumerated\"\n\
                  hierarchy                 = { kind = \"flat\" }\nvisibility                = \"public\"\n\
                  artifact_visibility       = { default = \"inherited\" }\n\
                  require_member_visibility = \"none\"\ndepends_on                = [\"clusters/a\"]\n";
    let config = parse_str(&with_layer(second)).expect("two layers parse");
    assert_eq!(
        config.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
        ["clusters/a", "topics/x"]
    );
}
