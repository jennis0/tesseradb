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
///
/// **`[sources]` names files nothing here reads**, and that is what a source table is: the paths
/// live in one place and the blocks that want one name it. `LAYER` and `SUGAR` below are appended
/// to this fixture and name these.
const SEVERITY: &str = r#"
[sources]
hdbscan         = "hdbscan.parquet"
hdbscan_members = "hdbscan_members.parquet"
topics          = "topics.parquet"
topic_members   = "topic_members.parquet"
other           = "other.parquet"

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

/// [`LAYER`] with the two things a **predicate** layer may not declare taken out: the proportional
/// criterion, whose denominator is ⊘ unruled over a rule-defined membership, and the computed
/// content, which needs one artifact's own membership and has no cheap route to it. The membership
/// line is left as `"enumerated"` for the caller to substitute, so every case below differs from
/// its neighbours in exactly the membership.
fn predicate_fixture() -> String {
    // The predicate reads the entity-addressed value column `index = true` writes, so the fixture's
    // category column carries one here.
    with_layer("")
        .replace(
            "type       = \"category\"\nvocabulary = \"severity\"",
            "type       = \"category\"\nindex      = true\nvocabulary = \"severity\"",
        )
        .replace(
            "require_member_visibility = { fraction = 0.05 }",
            "require_member_visibility = { count = 3 }",
        )
        .replace("  computed = [\"centroid\", \"box\"]\n", "")
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
/// `[layer.labels]` is in it on the same footing as everything else: the sugar expands to a
/// `[[layer]]` before anything compiles, so its key set is a real surface a caller declares
/// against and a key added to it without an entry in §1 is a control nobody has reasoned about.
#[test]
fn the_accepted_key_set_is_configuration_ms_table() {
    expect_keys(
        "nonesuch = 1\n",
        "the document",
        &[
            "sources",
            "defaults",
            "view",
            "vocabulary",
            "attribute",
            "layer",
        ],
    );
    expect_keys(
        "[defaults]\nnonesuch = 1\n",
        "[defaults]",
        &["source", "entity_id_field"],
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
            "source",
            "entity_id_field",
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
            "value_set",
            "hierarchy",
            "layout",
            "visibility",
            "artifact_visibility",
            "require_member_visibility",
            "withdraw_on_member_deletion",
            "depends_on",
            "levels",
            "content",
            "members",
            "labels",
            "shape",
        ],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\nartifacts = [{ nonesuch = 1 }]\n",
        "an inline artifact",
        &[
            "key",
            "level",
            "members",
            "excluding",
            "bbox",
            "contents",
            "parent",
            "attached_layer",
            "attached_level",
            "attached_key",
        ],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\n[layer.shape]\nnonesuch = 1\n",
        "[layer.shape]",
        &["kind", "depth"],
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
    expect_keys(
        "[[layer]]\nname = \"l\"\n[layer.labels.content]\nnonesuch = 1\n",
        "[layer.labels.content]",
        &["require_member_visibility"],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\n[layer.labels]\nnonesuch = 1\n",
        "[layer.labels]",
        &[
            "artifact_visibility",
            "content",
            "name",
            "title",
            "source",
            "fields",
            "members",
            "type",
            "membership",
            "require_member_visibility",
            "visibility",
        ],
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
    let text =
        "[[layer]]\nname = \"l\"\n[layer.content]\non_member_deletion = \"withdraw_content\"\n";
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
    assert_eq!(vocab.visibility, Visibility::Public);
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
    let text = SEVERITY.replace(
        "visibility = \"public\"",
        "visibility = \"public\"\nreserved = [2]",
    );
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
        Visibility::Derived
    );

    for word in ["ir:analyst", "per_viewer", "inherited", "none", "Public"] {
        let text = SEVERITY.replace(
            "visibility = \"public\"",
            &format!("visibility = \"{word}\""),
        );
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
    assert!(parse_str(SEVERITY)
        .unwrap()
        .schema
        .open_minters()
        .is_empty());
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
        let text = format!(
            "[[attribute]]\nname = \"measure\"\ntype = \"{ty}\"\nrender = true\nindex = true\n"
        );
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

    let message =
        err("[[attribute]]\nname = \"abstract\"\ntype = \"text\"\nanalyser = \"standard\"\n");
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
    assert!(
        message.contains("`point_visibility` is required"),
        "{message}"
    );
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
        assert!(
            message.contains(spelling),
            "{spelling} missing from: {message}"
        );
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
    assert!(
        message.contains("not a fraction of the data span"),
        "{message}"
    );
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
    assert!(!layer.artifact_visibility.carries_own_labels());
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
        (
            "visibility                = \"public\"\n",
            "`visibility` is required",
        ),
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
        (
            "{ fraction = 0.1 }",
            Some(ExistenceCriterion::Fraction(0.1)),
        ),
    ] {
        let text = with_layer("").replace(
            "require_member_visibility = { fraction = 0.05 }",
            &format!("require_member_visibility = {written}"),
        );
        let config = parse_str(&text).unwrap_or_else(|e| panic!("{written}: {e}"));
        assert_eq!(
            config.layers[0].require_member_visibility, expected,
            "{written}"
        );
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
        .replace(
            "  [vocabulary.values]\n  low = 1\n  high = 2\n",
            "values = []\n",
        )
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
    let text = with_layer("").replace(
        "visibility                = \"public\"",
        "visibility                = \"inherited\"",
    );
    let message = err(&text);
    assert!(
        message.contains("may not be spelled `inherited`"),
        "{message}"
    );
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
    assert!(config.layers[0].artifact_visibility.carries_own_labels());
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
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s7\"]",
    );
    let message = err(&text);
    assert!(message.contains("s7"), "{message}");
    assert!(message.contains("Declared: s0"), "{message}");
}

#[test]
fn a_layer_declares_its_hierarchy_rather_than_having_it_inferred() {
    let text = with_layer("").replace(
        "hierarchy                 = { kind = \"flat\", prune_children = true }\n",
        "",
    );
    let message = err(&text);
    assert!(message.contains("`hierarchy` is required"), "{message}");
    assert!(
        message.contains("tiered"),
        "the four kinds must be named: {message}"
    );

    let text = with_layer("").replace("kind = \"flat\"", "kind = \"treed\"");
    assert!(err(&text).contains("none of"), "{}", err(&text));
}

/// The levels rule follows from the kind, and the check is the registry's own — one implementation
/// of the rules, fired at parse rather than part-way through a build.
#[test]
fn the_levels_rule_follows_the_declared_kind() {
    let levels = "\n[[layer.levels]]\nlevel = 0\ntitle = \"countries\"\n";
    let nested = with_layer("").replace("kind = \"flat\"", "kind = \"nested\"");
    assert!(
        parse_str(&nested).is_ok(),
        "a nested layer declares no levels"
    );
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
    let message = err(&format!(
        "{base}\n[[layer.levels]]\ntitle = \"countries\"\n"
    ));
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
    assert!(content.require_member_visibility.requires_all_members());

    let without = "\n[[layer.content.supplied]]\nname = \"topic\"\ntype = \"text\"\n";
    let message = err(&with_layer(without));
    assert!(
        message.contains("no `require_member_visibility`"),
        "{message}"
    );
    assert!(message.contains("C28"), "{message}");

    let no_type =
        "\n[[layer.content.supplied]]\nname = \"topic\"\nrequire_member_visibility = \"all\"\n";
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
    assert!(
        message.contains("annotation-write-cycle.md §6.1"),
        "{message}"
    );
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
    assert!(
        parse_str(&with_layer("")).unwrap().layers[0]
            .content
            .withdraw_on_member_deletion
    );
}

/// `membership` at its three spellings, and the ways of getting each wrong.
///
/// **Neither table is decoration.** An attribute membership is a predicate over one value column
/// and which column is part of the declaration; a spatial membership is a box covered by tiles of a
/// declared depth, and *the depth is the membership* — a box covered at depth 4 and the same box at
/// depth 8 hold different points. So both bare words, each of which was a spelling before its value
/// had a home, are refused with the table to write instead rather than as unknown words
/// (decision 0048: replaced, not aliased).
#[test]
fn a_layer_declares_its_membership_source() {
    let text = with_layer("").replace("membership                = \"enumerated\"\n", "");
    assert!(
        err(&text).contains("`membership` is required"),
        "{}",
        err(&text)
    );
    let text = with_layer("").replace("\"enumerated\"", "\"predicate\"");
    assert!(err(&text).contains("neither"), "{}", err(&text));

    let predicate = predicate_fixture();

    let spatial = predicate.replace("\"enumerated\"", "\"spatial\"");
    assert_eq!(
        parse_str(&spatial).unwrap().layers[0].membership,
        MembershipSource::Spatial
    );
    // ⊘ And with no `[layer.shape]` it carries none — the state this surface has always had, a
    // layer declared for a shape it does not yet hold.
    assert_eq!(parse_str(&spatial).unwrap().layers[0].shape, None);

    let attribute = predicate.replace("\"enumerated\"", "{ attribute = \"severity\" }");
    assert_eq!(
        parse_str(&attribute).unwrap().layers[0].membership,
        MembershipSource::Attribute("severity".to_string())
    );

    // The two retired bare words, each named rather than reported as an unknown value: a caller who
    // wrote one is looking for the value it now carries, not for a fourth kind of membership.
    let bare = predicate.replace("\"enumerated\"", "\"attribute\"");
    assert!(err(&bare).contains("names no column"), "{}", err(&bare));

    let empty = predicate.replace("\"enumerated\"", "{ attribute = \"\" }");
    assert!(
        err(&empty).contains("must name the value column"),
        "{}",
        err(&empty)
    );
    let two = predicate.replace("\"enumerated\"", "{ attribute = \"a\", listed = true }");
    assert!(
        err(&two).contains("takes exactly `attribute`"),
        "{}",
        err(&two)
    );

    // A column nothing declares reads nothing, and the layer would publish empty memberships —
    // refused at the declaration, before a data file is opened, on the rule an attribute naming
    // an undeclared vocabulary already follows.
    let absent = predicate.replace("\"enumerated\"", "{ attribute = \"nonesuch\" }");
    let message = err(&absent);
    assert!(message.contains("names no declared attribute"), "{message}");
    assert!(message.contains("severity"), "{message}");
}

/// **The layout pin: three words, and a pin the build ignored would be the silent case.**
///
/// Absent is the normal state and compiles to `None` — the automatic pick, re-evaluated at every
/// fold. A word outside the three is refused rather than reported as an unknown value, because a
/// caller who wrote one is choosing a storage form and needs to be told which forms there are.
#[test]
fn a_layer_may_pin_its_serving_layout() {
    assert_eq!(parse_str(&with_layer("")).unwrap().layers[0].layout, None);

    for (word, expected) in [
        ("rows", ServingLayout::ArtifactMajor),
        ("column", ServingLayout::RowMajorLabel),
        ("list", ServingLayout::RowMajorList),
    ] {
        let text = with_layer("").replace(
            "membership                = \"enumerated\"\n",
            &format!("membership                = \"enumerated\"\nlayout                    = \"{word}\"\n"),
        );
        assert_eq!(
            parse_str(&text).unwrap().layers[0].layout,
            Some(expected),
            "layout = \"{word}\""
        );
    }

    let bogus = with_layer("").replace(
        "membership                = \"enumerated\"\n",
        "membership                = \"enumerated\"\nlayout                    = \"row-major\"\n",
    );
    let message = err(&bogus);
    assert!(message.contains("is not a layout"), "{message}");
    assert!(message.contains("column"), "{message}");

    // **A predicate layer's form follows from its membership, so every pin is refused** — refused
    // here, by the same `validate` the online registration calls.
    for word in ServingLayout::PIN_VOCABULARY {
        let spatial = predicate_fixture().replace(
            "membership                = \"enumerated\"\n",
            &format!(
                "membership                = \"spatial\"\nlayout                    = \"{word}\"\n"
            ),
        );
        assert!(
            err(&spatial).contains("per-row source"),
            "{}",
            err(&spatial)
        );
        let attribute = predicate_fixture().replace(
            "membership                = \"enumerated\"\n",
            &format!(
                "membership                = {{ attribute = \"severity\" }}\nlayout                    = \"{word}\"\n"
            ),
        );
        assert!(
            err(&attribute).contains("a layout pin"),
            "{}",
            err(&attribute)
        );
    }
}

/// `[layer.shape]` — what a spatial layer's artifacts are shaped like, and how deep they are drawn.
///
/// **The depth is the membership rather than a tuning key**, so there is no value for it to default
/// to and none outside the Morton code space to accept — a box covered at depth 4 and the same box
/// at depth 8 hold different points.
#[test]
fn a_spatial_layer_declares_its_shape_and_its_depth() {
    // The block is appended, because `[layer.shape]` is a sub-table of the `[[layer]]` above it and
    // a TOML sub-table header ends the key section it follows.
    let spatial = |body: &str| {
        format!(
            "{}{body}",
            predicate_fixture().replace(
                "membership                = \"enumerated\"",
                "membership                = \"spatial\"",
            )
        )
    };

    let declared = spatial("\n  [layer.shape]\n  kind = \"bbox\"\n  depth = 6\n");
    assert_eq!(
        parse_str(&declared).unwrap().layers[0].shape,
        Some(ShapeDeclaration {
            kind: ShapeKind::Bbox,
            depth: 6
        })
    );
    // One kind, so spelling it is a courtesy rather than a choice.
    let implied = spatial("\n  [layer.shape]\n  depth = 6\n");
    assert_eq!(
        parse_str(&implied).unwrap().layers[0].shape,
        Some(ShapeDeclaration {
            kind: ShapeKind::Bbox,
            depth: 6
        })
    );

    for body in [
        "\n  [layer.shape]\n  kind = \"bbox\"\n",
        "\n  [layer.shape]\n  depth = 0\n",
        "\n  [layer.shape]\n  depth = 17\n",
    ] {
        let text = spatial(body);
        assert!(
            err(&text).contains("must be an integer between 1 and 16"),
            "{}",
            err(&text)
        );
    }

    // ⊘ A polygon is refused rather than covered approximately: the tiles that cover a shape *are*
    // its membership, so an approximate cover is a membership wider than the declaration.
    let polygon = spatial("\n  [layer.shape]\n  kind = \"polygon\"\n  depth = 6\n");
    assert!(
        err(&polygon).contains("is not \"bbox\""),
        "{}",
        err(&polygon)
    );

    // A shape beside a membership that reads none is a rule nothing evaluates.
    let enumerated = format!("{}\n  [layer.shape]\n  depth = 6\n", predicate_fixture());
    assert!(
        err(&enumerated).contains("is a rule nothing evaluates"),
        "{}",
        err(&enumerated)
    );
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
/// **Every `source` is a name and every path is written once**, in `[sources]`, relative to the
/// declaring document (`configuration.md` §3) — which is what lets one file describe a corpus on a
/// laptop and in CI without an invocation naming five files.
const ACQUIRED: &str = r#"
[sources]
corpus   = "corpus.parquet"
geometry = "geometry.parquet"
pairs    = "pairs.parquet"
hdbscan  = "hdbscan.parquet"
hdbscan_members = "hdbscan_members.parquet"
topics   = "topics.parquet"
topic_members = "topic_members.parquet"
severity_values = "severity.parquet"
other    = "other.parquet"

[defaults]
source = "corpus"

[[view]]
name             = "s0"
extent           = "auto"
source           = "geometry"
point_visibility = { source = "pairs", default = "public" }

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

/// `--file NAME=PATH`, as the CLI hands it over: an override keyed by the **source's own name**,
/// so one of them moves every block reading that file. The paths need not exist — every case here
/// is refused, or answered, before a data file is opened.
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

fn bound_ok(text: &str, keys: &[&str]) -> Config {
    parse_bound(text, &files(keys)).expect("expected a parse")
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
    assert_eq!(config.attribute_sources.len(), 1);
    assert_eq!(
        config.attribute_sources[0].path,
        dir.path().join("corpus.parquet")
    );
    assert_eq!(
        config.views[0].source,
        Some(dir.path().join("geometry.parquet"))
    );
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
    let text = ACQUIRED.replace(
        "geometry = \"geometry.parquet\"",
        "geometry = \"/mnt/scratch/geometry.parquet\"",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("is an absolute path"), "{message}");
    assert!(message.contains("relative to this config"), "{message}");
    assert!(
        message.contains("--file geometry=/mnt/scratch/geometry.parquet"),
        "the refusal must name where an absolute path does belong: {message}"
    );
}

/// `--file` **overrides one source**, keyed by the object whose source it is.
#[test]
fn an_override_replaces_one_objects_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = parse_at(dir.path(), ACQUIRED, &files(&["geometry"]))
        .expect("an override of a declared source");
    assert_eq!(
        config.views[0].source,
        Some(PathBuf::from("/elsewhere/geometry.parquet"))
    );
    // Everything it did not name is still the declaration's own path.
    assert_eq!(
        config.attribute_sources[0].path,
        dir.path().join("corpus.parquet")
    );
}

/// **An override that names nothing is a refusal**, listing the keys that exist. Without this the
/// declaration's own path stays quietly in force under a command line asking for another corpus.
#[test]
fn an_override_no_object_declares_is_refused() {
    let message = bound_err(ACQUIRED, &["geomtery"]);
    assert!(message.contains("'geomtery=…'"), "{message}");
    assert!(
        message.contains("names no source in this declaration"),
        "{message}"
    );
    assert!(
        message.contains("geometry") && message.contains("corpus"),
        "the refusal must list the names that exist: {message}"
    );
}

/// An override **never creates** a source, so a closed vocabulary cannot be opened from the
/// command line — the fall-through §8 forbids, arriving through an invocation instead of a typo.
#[test]
fn an_override_is_never_a_fall_through_to_minting() {
    // `severity_values` is a declared path that the vocabulary does not name, so overriding it
    // moves a file nothing reads: the vocabulary still declares its values inline and is still
    // closed. An override moves a path; it never gives an object a `source` it did not write.
    let config = bound_ok(ACQUIRED, &["severity_values"]);
    let severity = &config.schema.vocabularies["severity"];
    assert_eq!(severity.value_set, ValueSet::Closed);
    assert_eq!(severity.codes.len(), 2, "still the two inline values");

    // …and a name `[sources]` does not carry is a refusal listing the ones it does.
    let message = bound_err(ACQUIRED, &["vocabulary:severity"]);
    assert!(
        message.contains("names no source in this declaration"),
        "{message}"
    );
    assert!(
        message.contains("never creates one"),
        "the refusal must say why an override cannot stand alone: {message}"
    );
}

/// **A `source` names a key of `[sources]`, and a name that table does not carry is refused** —
/// listing the names that do exist. There is no name-or-path fallback: an unmatched name read as a
/// relative path makes a typo a missing file rather than a declaration that does not resolve, and
/// the message then comes from a Parquet reader instead of from the document.
#[test]
fn a_source_naming_no_key_is_refused_listing_the_names_that_exist() {
    let text = ACQUIRED.replace(
        "source           = \"geometry\"",
        "source           = \"geomtery\"",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("`source = \"geomtery\"`"), "{message}");
    assert!(message.contains("names no key in `[sources]`"), "{message}");
    assert!(
        message.contains("geometry") && message.contains("pairs"),
        "the refusal must list the names that exist: {message}"
    );
    // …and the same for a name that happens to look like the path it used to be.
    let text = ACQUIRED.replace(
        "source           = \"geometry\"",
        "source           = \"geometry.parquet\"",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("names no key in `[sources]`"), "{message}");
}

/// An **absolute** path is refused where paths live — `[sources]` — rather than at each block that
/// names the source, which is the same rule at the one place it can now be stated.
#[test]
fn an_absolute_path_in_sources_names_the_override() {
    let text = ACQUIRED.replace(
        "pairs    = \"pairs.parquet\"",
        "pairs    = \"/mnt/staged/pairs.parquet\"",
    );
    let message = bound_err(&text, &[]);
    assert!(
        message.contains("`[sources].pairs`") || message.contains("[sources].pairs"),
        "{message}"
    );
    assert!(message.contains("is an absolute path"), "{message}");
    assert!(
        message.contains("--file pairs=/mnt/staged/pairs.parquet"),
        "{message}"
    );
}

/// **One override moves every reader of a source at once**, which is the whole reason the key is
/// the source rather than the object: the object-keyed form needed one override per block and left
/// the one you missed quietly reading the old file.
#[test]
fn one_override_moves_every_reader_of_that_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The view and the attributes are made to read one source, as an ordinary corpus does.
    let text = ACQUIRED.replace("source           = \"geometry\"\n", "");
    let config = parse_at(dir.path(), &text, &files(&["corpus"]))
        .expect("an override of a source two blocks read");
    assert_eq!(
        config.views[0].source,
        Some(PathBuf::from("/elsewhere/corpus.parquet")),
        "the view took `[defaults].source` and moved with it"
    );
    assert_eq!(
        config.attribute_sources[0].path,
        PathBuf::from("/elsewhere/corpus.parquet"),
        "and so did every column reading it"
    );
}

/// **`[defaults]` supplies a source to a view and to a column, and nothing else** — because
/// elsewhere an absent source is itself a declaration.
#[test]
fn defaults_reach_a_view_and_a_column_and_no_other_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = ACQUIRED.replace("source           = \"geometry\"\n", "");
    let config = parse_at(dir.path(), &text, &HashMap::new()).expect("a parse");
    assert_eq!(
        config.views[0].source,
        Some(dir.path().join("corpus.parquet"))
    );
    assert_eq!(
        config.attribute_sources[0].path,
        dir.path().join("corpus.parquet")
    );

    // A vocabulary with no source mints rather than reads, and the default does not make it read.
    let open = text.replace(
        "value_set  = \"closed\"\nvisibility = \"public\"\nvalues     = [\"low\", \"high\"]",
        "value_set  = \"open\"\nvisibility = \"derived\"",
    );
    let config = parse_at(dir.path(), &open, &HashMap::new()).expect("a parse");
    assert!(
        config.schema.vocabularies["severity"].codes.is_empty(),
        "an open vocabulary with no source starts empty rather than reading the default file"
    );

    // A layer with no source is declared and empty, and stays so.
    let layered = format!(
        "{text}{}",
        LAYER.replace(
            "  [layer.content]\n  computed = [\"centroid\", \"box\"]\n",
            ""
        )
    );
    let config = parse_at(dir.path(), &layered, &HashMap::new()).expect("a parse");
    assert!(
        config.layer_sources[0].artifacts.is_none(),
        "a layer naming no source acquires none"
    );
}

/// **A column may name its own source and its own identity column**, which is what `[corpus]`
/// could not express: the entity id is what puts a value in this entity space, and the file it
/// arrived in never was.
#[test]
fn an_attribute_may_name_its_own_source_and_identity_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = format!(
        "{ACQUIRED}\n[[attribute]]\nname            = \"sentiment\"\nfield           = \"score\"\n\
         type            = \"f32\"\nsource          = \"other\"\nentity_id_field = \"doc_id\"\n"
    );
    let config = parse_at(dir.path(), &text, &HashMap::new()).expect("a parse");
    assert_eq!(config.attribute_sources.len(), 2, "two files, two passes");
    assert_eq!(config.attribute_sources[0].name, "corpus");
    assert_eq!(config.attribute_sources[0].attributes, vec![0]);
    assert_eq!(config.attribute_sources[1].name, "other");
    assert_eq!(config.attribute_sources[1].attributes, vec![1]);
    assert_eq!(
        config.attribute_sources[1].path,
        dir.path().join("other.parquet")
    );
    assert_eq!(config.attribute_sources[1].fields.of("entity_id"), "doc_id");
    // The first group joins on whatever this declaration spells identity, which is the canonical
    // name here because `[defaults]` says nothing else.
    assert_eq!(
        config.attribute_sources[0].fields.of("entity_id"),
        "entity_id"
    );
}

/// **Columns sharing a source share a pass**, in declaration order — which is load-bearing, the
/// scalar tail being stored positionally.
#[test]
fn columns_sharing_a_source_share_one_pass() {
    let text = format!(
        "{ACQUIRED}\n[[attribute]]\nname = \"a\"\ntype = \"u8\"\n\
         \n[[attribute]]\nname = \"b\"\ntype = \"u8\"\nsource = \"other\"\n\
         \n[[attribute]]\nname = \"c\"\ntype = \"u8\"\n"
    );
    let config = parse_str(&text).expect("a parse");
    assert_eq!(config.attribute_sources.len(), 2);
    assert_eq!(config.attribute_sources[0].name, "corpus");
    assert_eq!(
        config.attribute_sources[0].attributes,
        vec![0, 1, 3],
        "declaration order within the group, and a subsequence of it"
    );
    assert_eq!(config.attribute_sources[1].attributes, vec![2]);
}

/// **`[defaults].entity_id_field` says how this declaration spells identity**, and every block
/// that reads one may say otherwise.
#[test]
fn the_identity_column_defaults_once_and_each_reader_may_override_it() {
    let text = ACQUIRED.replace(
        "[defaults]\nsource = \"corpus\"",
        "[defaults]\nsource = \"corpus\"\nentity_id_field = \"id\"",
    );
    let config = bound_ok(&text, &[]);
    assert_eq!(config.views[0].fields.of("entity_id"), "id");
    assert_eq!(config.attribute_sources[0].fields.of("entity_id"), "id");

    // The view says otherwise through its own map…
    let moved = text.replace(
        "source           = \"geometry\"",
        "source           = \"geometry\"\nfields           = { entity_id = \"gid\" }",
    );
    let config = bound_ok(&moved, &[]);
    assert_eq!(config.views[0].fields.of("entity_id"), "gid");
    assert_eq!(config.attribute_sources[0].fields.of("entity_id"), "id");

    // …and a column through its own key.
    let moved = text.replace(
        "vocabulary = \"severity\"",
        "vocabulary = \"severity\"\nentity_id_field = \"doc_id\"",
    );
    let config = bound_ok(&moved, &[]);
    assert_eq!(config.attribute_sources[0].fields.of("entity_id"), "doc_id");
    assert_eq!(config.views[0].fields.of("entity_id"), "id");
}

/// `[defaults].source` naming nothing is refused once, quoting `[defaults]`, rather than once per
/// block that took it.
#[test]
fn a_default_source_naming_no_key_is_refused() {
    let text = ACQUIRED.replace(
        "[defaults]\nsource = \"corpus\"",
        "[defaults]\nsource = \"corpsu\"",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("[defaults]"), "{message}");
    assert!(message.contains("names no key in `[sources]`"), "{message}");
}

/// A value set is inline **or** sourced: two spellings of one thing, so both is a parse error
/// rather than a precedence question.
#[test]
fn inline_values_and_a_source_together_are_refused() {
    let text = ACQUIRED.replace(
        "values     = [\"low\", \"high\"]",
        "values     = [\"low\", \"high\"]\nsource     = \"severity_values\"",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("spellings of one thing"), "{message}");
}

/// **The map says *where*, never *whether*.** A name outside the object's fields is refused
/// rather than passed to the reader.
#[test]
fn a_field_map_may_not_name_a_field_the_object_does_not_have() {
    let text = ACQUIRED.replace(
        "source           = \"geometry\"",
        "source           = \"geometry\"\nfields           = { nonesuch = \"nonesuch\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("`fields.nonesuch`"), "{message}");
    assert!(
        message.contains("not one of this object's fields"),
        "{message}"
    );
    assert!(
        message.contains("entity_id"),
        "the refusal must list them: {message}"
    );
}

/// A field the object never declared is refused, and the refusal names the key that *would*
/// declare it — locating a field is not a way to assert one exists.
#[test]
fn a_field_map_may_not_name_a_field_the_object_never_declared() {
    // `parent` on a flat layer: the hierarchy kind is what says there are lineage edges.
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nfields                    = { parent = \"parent\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("never declared"), "{message}");
    assert!(message.contains("hierarchy.kind"), "{message}");

    // `attached_key` with no `depends_on`: an edge points into a layer this one never named.
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nfields                    = { attached_key = \"attached_key\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("depends_on"), "{message}");

    // `contents` with no supplied content declared.
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nfields                    = { contents = \"contents\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("layer.content.supplied"), "{message}");
}

/// A map with no source names the fields of nothing. A vocabulary, because `[defaults].source`
/// deliberately does not reach one — an absent vocabulary source is a set that mints rather than
/// reads, so there is genuinely no file for the map to locate anything in.
#[test]
fn a_field_map_without_a_source_is_refused() {
    let text = ACQUIRED.replace(
        "values     = [\"low\", \"high\"]",
        "values     = [\"low\", \"high\"]\nfields     = { key = \"k\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("`fields` without a `source`"), "{message}");
}

/// A map *moves* a field, and the reader takes the name it moved it to.
#[test]
fn a_renamed_field_reaches_the_reader() {
    let text = ACQUIRED.replace(
        "vocabulary = \"severity\"",
        "vocabulary = \"severity\"\nentity_id_field = \"id\"",
    );
    let config = bound_ok(&text, &[]);
    assert_eq!(config.attribute_sources[0].fields.of("entity_id"), "id");

    // An attribute's own one-field map is the same rule, spelled for one field.
    let text = with_line(SEVERITY, "field = \"sev\"");
    let config = parse_str(&text).expect("expected a parse");
    let severity = config
        .schema
        .attributes
        .iter()
        .find(|a| a.name == "severity")
        .expect("the attribute is declared");
    assert_eq!(severity.column(), "sev");
    // The served name is untouched: the map says where the column is, not what it is called.
    assert_eq!(severity.name, "severity");
}

/// A field map entry naming no column at all is refused rather than read as *the canonical name*.
#[test]
fn an_empty_field_name_is_refused() {
    let text = ACQUIRED.replace(
        "source           = \"geometry\"",
        "source           = \"geometry\"\nfields           = { entity_id = \"\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("names no column"), "{message}");

    let text = with_line(SEVERITY, "field = \"  \"");
    let message = err(&text);
    assert!(message.contains("names no column"), "{message}");
}

/// **A layer's map moves a field, and the reader takes the name it moved it to** — the same rule
/// every other object's map follows now that a layer reads its own source.
#[test]
fn a_layer_field_map_resolves_to_the_column_it_names() {
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nfields                    = { key = \"cluster_id\" }",
    );
    let config = bound_ok(&text, &[]);
    let Some(crate::config::ArtifactSource::File { fields, .. }) =
        &config.layer_sources[0].artifacts
    else {
        panic!("the layer names a file");
    };
    assert_eq!(fields.of("key"), "cluster_id");
    assert_eq!(
        fields.of("contents"),
        "contents",
        "an unmoved field keeps its own name"
    );
}

/// **A membership is included or excluded, never both.** The two are one field written two ways,
/// and the build complements the second into the first — so naming each leaves two memberships for
/// one artifact, and every masked count divides by one of them.
#[test]
fn a_layer_naming_both_members_and_excluding_is_refused() {
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nfields                    = { members = \"m\", excluding = \"x\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("two spellings of one field"), "{message}");
}

/// **A layer's artifacts come from its file or from the declaration, never both.** Two answers to
/// what its artifacts are would be resolved by nothing the caller wrote.
#[test]
fn a_layer_declaring_a_source_and_inline_artifacts_is_refused() {
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nartifacts                 = [{ key = \"c-0\" }]",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("two spellings of one thing"), "{message}");
}

/// An inline artifact is already on the canonical names, so there is no file for a `fields` map to
/// locate anything in.
#[test]
fn a_field_map_beside_inline_artifacts_is_refused() {
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nartifacts                 = [{ key = \"c-0\" }]\nfields                    = { key = \"cluster_id\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("no file for the names"), "{message}");
}

/// The same rule on the inline row: one membership per artifact, whichever way it is spelled.
#[test]
fn an_inline_artifact_declaring_both_members_and_excluding_is_refused() {
    let text = with_layer("").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nartifacts                 = [{ key = \"c-0\", members = [1], excluding = [2] }]",
    );
    let message = bound_err(&text, &[]);
    assert!(
        message.contains("two spellings of one membership"),
        "{message}"
    );
}

/// **A comma is an ordinary byte in an access label.** The build hands the plugin a term *list*,
/// so a declared label spelling `ir:analyst,ir:legal` is one term reaching one principal — the
/// holder of that whole string, never the holder of either half.
#[test]
fn an_access_label_may_contain_a_comma_and_is_one_term() {
    let text = ACQUIRED.replace("default = \"public\"", "default = \"ir:analyst,ir:legal\"");
    let config = bound_ok(&text, &[]);
    assert_eq!(
        config.views[0].point_visibility.default, "ir:analyst,ir:legal",
        "carried whole, not split at the comma"
    );
}

/// **A point's label comes from a field or from a source, never both** (§1).
#[test]
fn a_point_label_comes_from_a_field_or_a_source_never_both() {
    let text = ACQUIRED.replace(
        "point_visibility = { source = \"pairs\", default = \"public\" }",
        "point_visibility = { source = \"pairs\", field = \"categories\", default = \"public\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(
        message.contains("both a `field` and a `source`"),
        "{message}"
    );
}

/// The two geometry shapes are mutually exclusive: a row carries coordinates or a code.
#[test]
fn the_two_geometry_shapes_may_not_both_be_located() {
    let text = ACQUIRED.replace(
        "source           = \"geometry\"",
        "source           = \"geometry\"\nfields           = { x = \"x\", morton = \"morton\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("mutually exclusive"), "{message}");

    let text = ACQUIRED.replace(
        "source           = \"geometry\"",
        "source           = \"geometry\"\nfields           = { residual = \"residual\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(message.contains("without `fields.morton`"), "{message}");
}

/// A layer's membership has two shapes and it names whichever it uses — never both.
#[test]
fn membership_is_a_list_field_or_a_source_never_both() {
    let text = with_layer("  [layer.members]\n  source = \"hdbscan_members\"\n").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\nfields                    = { members = \"members\" }",
    );
    let message = bound_err(&text, &[]);
    assert!(
        message.contains("membership is declared twice"),
        "{message}"
    );
}

/// A member source with no roster would make every mistyped key its own artifact.
#[test]
fn a_member_source_needs_the_layers_own_source() {
    let text = with_layer("  [layer.members]\n  source = \"hdbscan_members\"\n");
    let message = bound_err(&text, &[]);
    assert!(message.contains("roster"), "{message}");
    // …and the refusal names the way to ask for exactly that.
    assert!(message.contains("value_set"), "{message}");
}

/// **`value_set` decides whether a member key may create an artifact**
/// (`artifacts-from-points.md` §3). Closed is the default and is the roster rule; open makes the
/// artifacts source enrichment, so a member source with no roster at all is then a declaration
/// rather than a mistake — a bare clustering, whose clusters exist because its points name them.
#[test]
fn a_layers_value_set_decides_whether_a_key_may_create_an_artifact() {
    let closed = parse_str(&with_layer("")).expect("a layer parses");
    assert_eq!(closed.layers[0].value_set, ValueSet::Closed);

    let declared = |word: &str, extra: &str| {
        format!(
            "{}{extra}",
            with_layer("").replace(
                "membership                = \"enumerated\"",
                &format!("membership                = \"enumerated\"\nvalue_set = \"{word}\""),
            )
        )
    };
    let open = parse_str(&declared(
        "open",
        "\n  [layer.members]\n  source = \"hdbscan_members\"\n",
    ))
    .expect("an open layer needs no roster");
    assert_eq!(open.layers[0].value_set, ValueSet::Open);

    let message = err(&declared("partial", ""));
    assert!(message.contains("neither"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// `[layer.labels]` — the sugar, and the layer it is sugar for
// ---------------------------------------------------------------------------------------------

/// The sugar, written under `LAYER`'s clustering.
const SUGAR: &str = r#"
  [layer.labels]
  name                      = "topics/a"
  title                     = "topics"
  source                    = "topics"
  fields                    = { members = "documents" }
  type                      = "text"
  membership                = "enumerated"
  require_member_visibility = { fraction = 0.05 }
  artifact_visibility       = { default = "inherited" }

    [layer.labels.content]
    require_member_visibility = "all"
"#;

/// The same layer, written out — every key the expansion supplies, spelled by hand.
const WRITTEN_OUT: &str = r#"
[[layer]]
name                      = "topics/a"
title                     = "topics"
views                     = ["s0"]
source                    = "topics"
fields                    = { members = "documents" }
membership                = "enumerated"
hierarchy                 = { kind = "flat", prune_children = false }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }
depends_on                = ["clusters/a"]

  [[layer.content.supplied]]
  name                      = "topics/a"
  type                      = "text"
  require_member_visibility = "all"
"#;

/// **The definition of sugar, asserted**: the two spellings compile to the same declaration and
/// the same bound sources, so nothing downstream of the parser can tell them apart. The bundle
/// half of the same claim is `build_layers.rs`'s
/// `the_label_sugar_and_the_layer_written_out_build_the_same_bundle`.
#[test]
fn the_sugar_and_the_layer_written_out_are_one_declaration() {
    // One directory for both, since a bound source is an absolute path and a `tempdir` per parse
    // would differ in exactly the field this is asserting is the same.
    let dir = tempfile::tempdir().expect("tempdir");
    let sugared = parse_at(dir.path(), &with_layer(SUGAR), &HashMap::new())
        .expect("the sugared declaration parses");
    let written = parse_at(dir.path(), &with_layer(WRITTEN_OUT), &HashMap::new())
        .expect("the written-out declaration parses");
    assert_eq!(sugared.layers, written.layers);
    assert_eq!(
        format!("{:?}", sugared.layer_sources),
        format!("{:?}", written.layer_sources)
    );
    // And what it expanded to, stated rather than left implicit in the equality above.
    let label = &sugared.layers[1];
    assert_eq!(label.name, "topics/a");
    assert_eq!(label.views, vec!["s0".to_string()]);
    assert_eq!(label.hierarchy.kind, HierarchyKind::Flat);
    assert_eq!(label.depends_on, vec!["clusters/a".to_string()]);
    assert_eq!(label.levels, Vec::new());
    assert_eq!(label.content.supplied.len(), 1);
    assert_eq!(label.content.supplied[0].name, "topics/a");
    assert_eq!(label.content.supplied[0].ty, "text");
    assert_eq!(
        label.artifact_visibility,
        tessera_types::layer::ArtifactVisibility {
            field: None,
            default: MemberDefault::Inherited
        }
    );
}

/// **The one defaulted disclosure control in the surface.**
///
/// The default is the parent's own value — never the widest one — so a label layer under a gated
/// clustering is gated the same way without a word. What a *declared* gate is not is compared
/// against the parent's: a dependency edge makes a label visible only where its cluster is visible
/// (decision 0089), so no gate written here can widen what a principal is shown, and the refusal
/// this expansion once carried for `public` under a gated parent is gone with the property that
/// replaced it.
#[test]
fn a_label_layers_gate_defaults_to_its_parents_and_any_declared_gate_is_taken() {
    let public_parent = parse_str(&with_layer(SUGAR)).unwrap();
    assert_eq!(public_parent.layers[0].visibility, None);
    assert_eq!(public_parent.layers[1].visibility, None);

    let gated = with_layer(SUGAR).replace(
        "visibility                = \"public\"",
        "visibility                = \"ir:analyst\"",
    );
    let config = parse_str(&gated).expect("a gated parent, and the label layer takes its gate");
    assert_eq!(
        config.layers[1].visibility,
        Some("ir:analyst".to_string()),
        "the default is the parent's actual gate"
    );

    // Declared narrower — admitted, and deliberately not ordered against the parent's: two opaque
    // terms carry no ordering the build could compute (`expand_labels`).
    let narrower = gated.replace(
        "  membership                = \"enumerated\"",
        "  membership                = \"enumerated\"\n  visibility = \"ir:secret\"",
    );
    assert_eq!(
        parse_str(&narrower).unwrap().layers[1].visibility,
        Some("ir:secret".to_string())
    );

    // And `public` under a gated parent, which was the one refusal here: admitted now, because a
    // viewer who cannot reach the cluster cannot reach its labels whatever this says.
    let wider = gated.replace(
        "  membership                = \"enumerated\"",
        "  membership                = \"enumerated\"\n  visibility = \"public\"",
    );
    assert_eq!(
        parse_str(&wider)
            .expect("a public label layer under a gated parent is a declaration, not a leak")
            .layers[1]
            .visibility,
        None,
        "`public` compiles to no gate at all, which is what it means"
    );
}

/// The reserved word still collides in the sugar's own gate slot, which is a slot that takes a
/// caller's label.
#[test]
fn an_access_label_spelled_inherited_is_refused_in_the_sugar_too() {
    let text = with_layer(SUGAR).replace(
        "  membership                = \"enumerated\"",
        "  membership                = \"enumerated\"\n  visibility = \"inherited\"",
    );
    let message = err(&text);
    assert!(
        message.contains("may not be spelled `inherited`"),
        "{message}"
    );
}

/// What the sugar never supplies is what a caller must write. It fills in the mechanical keys — the
/// views, the flat hierarchy, the dependency on the parent, the content wrapper — and **not one
/// disclosure control**: both member requirements and the artifact gate are the caller's, and the
/// layer gate is the single defaulted key in the surface because the value it takes is the
/// parent's own.
///
/// The two requirements are separate keys because they ask different questions — a threshold on
/// how much of the set a viewer must see, and a declaration of where the text came from — which is
/// why neither can carry the other.
#[test]
fn the_sugar_supplies_the_mechanical_keys_and_no_disclosure_control() {
    for (removed, expected) in [
        (
            "  type                      = \"text\"\n",
            "`type` is required",
        ),
        (
            "  membership                = \"enumerated\"\n",
            "`membership` is required",
        ),
        (
            "  require_member_visibility = { fraction = 0.05 }\n",
            "`require_member_visibility` is required",
        ),
        (
            "  artifact_visibility       = { default = \"inherited\" }\n",
            "`artifact_visibility` is required",
        ),
        (
            "    require_member_visibility = \"all\"\n",
            "`[layer.labels.content]` must declare",
        ),
    ] {
        let text = with_layer(SUGAR).replace(removed, "");
        let message = err(&text);
        assert!(
            message.contains(expected)
                && (message.contains("topics/a") || message.contains("[layer.labels]")),
            "removing `{removed}` must be refused naming the label layer: {message}"
        );
    }
}

/// A label layer is a layer, so it collides with one: the name is the identity, and two layers
/// under one name would give bookmarks, edges and suppressions two destinations.
#[test]
fn a_label_layer_sharing_a_name_with_a_layer_is_refused() {
    let text = with_layer(SUGAR).replace(
        "name                      = \"topics/a\"",
        "name                      = \"clusters/a\"",
    );
    let message = err(&text);
    assert!(message.contains("declared twice"), "{message}");
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
    assert!(
        matches!(&acquired.access.source, crate::config::AccessSource::Relation(p) if *p == dir.path().join("pairs.parquet")),
        "{:?}",
        acquired.access
    );
    assert_eq!(
        acquired.attribute_sources[0].path,
        dir.path().join("corpus.parquet")
    );
    assert!(acquired.layers.is_empty());
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
        "{ACQUIRED}{}\n  [layer.members]\n  source = \"hdbscan_members\"\n",
        LAYER.replace(
            "views                     = [\"s0\"]",
            "views                     = [\"s0\"]\nsource                    = \"hdbscan\"",
        )
    );
    let config =
        parse_at(dir.path(), &text, &HashMap::new()).expect("the layer's two sources are declared");
    let acquired = config.acquire("s0").unwrap();
    assert_eq!(
        artifact_path(&acquired.layers[0]),
        Some(dir.path().join("hdbscan.parquet"))
    );
    assert_eq!(
        acquired.layers[0].members.as_ref().map(|m| m.path.clone()),
        Some(dir.path().join("hdbscan_members.parquet"))
    );
    // And the members source is overridable on its own name, without disturbing the roster.
    let config = parse_at(dir.path(), &text, &files(&["hdbscan_members"]))
        .expect("one source staged elsewhere");
    let acquired = config.acquire("s0").unwrap();
    assert_eq!(
        artifact_path(&acquired.layers[0]),
        Some(dir.path().join("hdbscan.parquet"))
    );
    assert_eq!(
        acquired.layers[0].members.as_ref().map(|m| m.path.clone()),
        Some(PathBuf::from("/elsewhere/hdbscan_members.parquet"))
    );
}

/// The file one layer's artifacts are read from, where it names one.
fn artifact_path(layer: &crate::config::LayerSources) -> Option<PathBuf> {
    match &layer.artifacts {
        Some(crate::config::ArtifactSource::File { path, .. }) => Some(path.clone()),
        _ => None,
    }
}

/// **One source per layer.** Two layers naming two files is the ordinary case now: each reads its
/// own, and there is no discriminator column for either to select on.
#[test]
fn two_layers_read_their_own_files() {
    let second = LAYER.replace("clusters/a", "clusters/b").replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"other\"",
    );
    let first = LAYER.replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let text = format!("{ACQUIRED}{first}{second}");
    let config = parse_at(dir.path(), &text, &HashMap::new()).expect("both layers parse");
    let acquired = config.acquire("s0").expect("both sources acquire");
    let paths: Vec<Option<PathBuf>> = acquired.layers.iter().map(artifact_path).collect();
    assert_eq!(
        paths,
        vec![
            Some(dir.path().join("hdbscan.parquet")),
            Some(dir.path().join("other.parquet"))
        ]
    );
}

/// The build materialises one view and reads its source, so a `--view` it cannot find is a build
/// with no geometry rather than a default one.
#[test]
fn a_build_refuses_a_view_the_config_does_not_declare() {
    let config = parse_bound(ACQUIRED, &HashMap::new()).unwrap();
    let message = format!("{}", config.acquire("s9").expect_err("expected a refusal"));
    assert!(message.contains("--view 's9'"), "{message}");
    assert!(
        message.contains("s0"),
        "the refusal must list them: {message}"
    );
}

/// ⊘ A view declaring no source **and reaching no `[defaults].source`** is legal and means the
/// view is declared and empty — a bundle with no rows in it, which is not built.
#[test]
fn a_build_refuses_a_view_with_no_source() {
    let text = ACQUIRED
        .replace("source           = \"geometry\"\n", "")
        .replace("[defaults]\nsource = \"corpus\"\n", "");
    let config = parse_bound(&text, &HashMap::new()).unwrap();
    let message = format!("{}", config.acquire("s0").expect_err("expected a refusal"));
    assert!(
        message.contains("`source` is required to build"),
        "{message}"
    );
}

/// All three label routes acquire: a field of the view's own source, a separate exploded relation,
/// and a `default` alone — the corpus with no permission model, where every point takes it.
#[test]
fn every_label_route_acquires() {
    use crate::config::AccessSource;
    let field = ACQUIRED.replace(
        "{ source = \"pairs\", default = \"public\" }",
        "{ field = \"categories\", default = \"public\" }",
    );
    let config = parse_bound(&field, &HashMap::new()).unwrap();
    let acquired = config.acquire("s0").expect("a field route acquires");
    assert!(
        matches!(&acquired.access.source, AccessSource::Field(f) if f == "categories"),
        "{:?}",
        acquired.access
    );
    assert_eq!(acquired.access.default, "public");

    let only_default = ACQUIRED.replace(
        "{ source = \"pairs\", default = \"public\" }",
        "{ default = \"ir:analyst\" }",
    );
    let config = parse_bound(&only_default, &HashMap::new()).unwrap();
    let acquired = config.acquire("s0").expect("a default alone acquires");
    assert!(
        matches!(acquired.access.source, AccessSource::Default),
        "{:?}",
        acquired.access
    );
    assert_eq!(acquired.access.default, "ir:analyst");
}

/// An attribute with no source at all: legal to **declare** (§2), and refused at the build that
/// would have to read the column — naming the columns rather than a block, which is what one
/// corpus file for all of them could never do.
#[test]
fn a_build_refuses_an_attribute_with_no_source() {
    let text = ACQUIRED.replace("[defaults]\nsource = \"corpus\"\n", "");
    let config = parse_bound(&text, &HashMap::new())
        .expect("a column with no source is a legal declaration");
    assert!(
        config.attribute_sources.is_empty(),
        "no source named, so no group to read"
    );
    let message = format!("{}", config.acquire("s0").expect_err("expected a refusal"));
    assert!(message.contains("name no `source`"), "{message}");
    assert!(
        message.contains("severity"),
        "the refusal names the column: {message}"
    );
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
        config
            .layers
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        ["clusters/a", "topics/x"]
    );
}
