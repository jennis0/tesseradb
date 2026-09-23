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

fn ok(text: &str) -> Config {
    parse_str(text).expect("expected the declaration to compile")
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
            "view_group",
            "vocabulary",
            "attribute",
            "layer",
        ],
    );
    expect_keys(
        "[defaults]\nnonesuch = 1\n",
        "[defaults]",
        &["source", "entity_id_field", "allocation_view"],
    );
    expect_keys(
        "[[view]]\nname = \"s0\"\nnonesuch = 1\n",
        "[[view]]",
        &[
            "name",
            "title",
            "projection",
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
        &["auto", "margin", "min", "max", "x", "y", "lon", "lat"],
    );
    expect_keys(
        "[[view]]\nname = \"s0\"\npoint_visibility = { nonesuch = 1 }\n",
        "point_visibility",
        &["field", "source", "default"],
    );
    expect_keys(
        "[[view_group]]\nname = \"g\"\nnonesuch = 1\n",
        "[[view_group]]",
        &[
            "name",
            "title",
            "projection",
            "source",
            "fields",
            "extent",
            "point_visibility",
            "visibility",
            "members",
            "metadata",
            "view",
            "views",
        ],
    );
    expect_keys(
        "[[view_group]]\nname = \"g\"\n[view_group.views]\nnonesuch = 1\n",
        "[view_group.views]",
        &["source", "fields"],
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
            "analyser",
            "scope",
            "fields",
        ],
    );
    expect_keys(
        "[[layer]]\nname = \"l\"\nnonesuch = 1\n",
        "[[layer]]",
        &[
            "name",
            "title",
            "views",
            "scope",
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
            "default_space",
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
            "circle",
            "ellipse",
            "wkt",
            "space",
            "contents",
            "parent",
            "attached_layer",
            "attached_level",
            "attached_key",
            "access",
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
        &["computed", "supplied"],
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
        let message = err(&SEVERITY.replace(line, ""));
        assert!(message.contains(expected), "{message}");
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
        assert!(
            parse_str(&text).is_err(),
            "`{word}` is neither `public` nor `derived`"
        );
    }
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

#[test]
fn a_closed_vocabulary_may_start_with_no_values() {
    let text = SEVERITY.replace("  [vocabulary.values]\n  low = 1\n  high = 2\n", "");
    let config = parse_str(&text).unwrap();
    assert!(config.schema.vocabularies["severity"].codes.is_empty());
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
    assert!(err(&text).contains("severtiy"));
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

#[test]
fn one_attribute_name_may_not_be_declared_twice() {
    let text = format!(
        "{SEVERITY}\n[[attribute]]\nname = \"severity\"\ntype = \"category\"\nvocabulary = \"severity\"\n"
    );
    assert!(err(&text).contains("declared twice"), "{}", err(&text));
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
    assert_eq!(
        config.views[0].point_visibility.default.as_deref(),
        Some("public")
    );
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

    // `default` is optional (decision 0133): a view with a field and no default compiles, and
    // an unlabelled point is then refused at both entry points rather than filled.
    let text = SEVERITY.replace(
        "point_visibility = { field = \"categories\", default = \"public\" }",
        "point_visibility = { field = \"categories\" }",
    );
    let config = parse_str(&text).unwrap();
    assert_eq!(
        config.views[0].point_visibility.field.as_deref(),
        Some("categories")
    );
    assert_eq!(config.views[0].point_visibility.default, None);

    // Neither an acquisition key nor a default names a label for any point, so it is refused at
    // the declaration rather than at the first row.
    let text = SEVERITY.replace(
        "point_visibility = { field = \"categories\", default = \"public\" }",
        "point_visibility = {}",
    );
    let message = err(&text);
    assert!(
        message.contains("names no `field`, no `source` and no `default`"),
        "{message}"
    );
}

/// **A point may not inherit.** A point carrying no terms is in no posting list and so in no
/// principal's mask, so inheriting would have to *add* a term — which can only widen it.
#[test]
fn a_point_default_may_not_be_inherited() {
    let text = SEVERITY.replace("default = \"public\"", "default = \"inherited\"");
    let message = err(&text);
    assert!(message.contains("point_visibility.default"), "{message}");
}

/// A view's own gate is a list of labels the manifest records and `Engine::authorise` evaluates
/// (`views.md` §6, decision 0132): one label written as a string, several as a list, each
/// element one term taken verbatim. What is refused is a list the plugin cannot read, an empty
/// element, or a list naming no terms at all — a gate satisfied by nobody, the view being
/// reachable by no principal including the author.
#[test]
fn a_views_own_visibility_is_a_label_the_plugin_can_read() {
    let gate_of = |declared: &str| {
        let text = with_line(SEVERITY, "").replace(
            "name             = \"s0\"",
            &format!("name             = \"s0\"\nvisibility       = {declared}"),
        );
        ok(&text).views[0].visibility.clone()
    };
    assert_eq!(
        gate_of("\"ir:analyst\""),
        Some(vec!["ir:analyst".to_string()]),
        "one label reaches the compiled view as a one-element list"
    );
    assert_eq!(
        gate_of("\"finance,legal\""),
        Some(vec!["finance,legal".to_string()]),
        "a comma inside a label is part of the label: one term, not two"
    );
    assert_eq!(
        gate_of("[\"finance\", \"legal\"]"),
        Some(vec!["finance".to_string(), "legal".to_string()]),
        "a list declares one term per element"
    );
    assert_eq!(gate_of("[\"public\"]"), None, "`public` as the whole list is no gate");

    let refusal = |declared: &str| {
        err(&with_line(SEVERITY, "").replace(
            "name             = \"s0\"",
            &format!("name             = \"s0\"\nvisibility       = {declared}"),
        ))
    };
    // The rules are tested in `tessera_plugin::check_visibility`; here, that the build applies them.
    refusal("[]");
    refusal("[\"public\", \"finance\"]");
}

/// `public` is the documented default and the current behaviour, so writing it records nothing
/// that is not already true — and the fixture corpora write it for clarity.
#[test]
fn a_view_may_declare_the_public_gate_it_already_has() {
    let text = with_line(SEVERITY, "").replace(
        "name             = \"s0\"",
        "name             = \"s0\"\nvisibility       = \"public\"",
    );
    let config = parse_str(&text).expect("`public` is the default, written out");
    assert_eq!(config.views[0].visibility, None);
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

// ---------------------------------------------------------------------------------------------
// `projections.md` §2 and §4.2: what a projected view declares, and what it may not
// ---------------------------------------------------------------------------------------------

/// The declaration with `line` written in place of `SEVERITY`'s bare `extent = "auto"`.
fn projected(line: &str) -> String {
    SEVERITY.replace("extent           = \"auto\"", line)
}

fn view_of(line: &str) -> View {
    parse_str(&projected(line))
        .unwrap_or_else(|e| panic!("{line}: {e}"))
        .views[0]
        .clone()
}

/// **`none` is the default, so a corpus with no geography is never asked to name a projection**
/// (`projections.md` §5.3), and every other name comes from the closed set.
#[test]
fn a_view_declares_a_projection_from_the_closed_set() {
    assert_eq!(view_of("extent = \"auto\"").projection, Projection::None);
    for (name, want) in [
        ("web_mercator", Projection::WebMercator),
        ("equirectangular", Projection::PLATE_CARREE),
        ("plate_carree", Projection::PLATE_CARREE),
        ("gall_isographic", Projection::GALL_ISOGRAPHIC),
        ("none", Projection::None),
    ] {
        let view = view_of(&format!("projection = \"{name}\"\nextent = \"auto\""));
        assert_eq!(view.projection, want, "{name}");
    }
}

/// A projection outside the set is refused **listing the set**, because the arithmetic of a
/// projection is part of the stored format: there is no reading of an unknown name that could be
/// approximated safely.
#[test]
fn a_projection_outside_the_set_is_refused() {
    let message = err(&projected(
        "projection = \"lambert_cylindrical_equal_area\"\nextent = \"auto\"",
    ));
    // The message lists the projections that exist.
    for name in [
        "web_mercator",
        "equirectangular",
        "plate_carree",
        "gall_isographic",
        "none",
    ] {
        assert!(message.contains(name), "{name} missing from: {message}");
    }
}

/// **A projected view's frame is a box in longitude and latitude, or `auto`** (§4.2).
#[test]
fn a_projected_view_states_its_frame_in_degrees() {
    let view = view_of(
        "projection = \"web_mercator\"\nextent = { lon = [-8.6, 1.8], lat = [49.9, 60.9] }",
    );
    assert_eq!(
        view.extent,
        Extent::LonLat(LonLatBox {
            lon_min: -8.6,
            lon_max: 1.8,
            lat_min: 49.9,
            lat_max: 60.9
        })
    );
    let view = view_of("projection = \"web_mercator\"\nextent = \"auto\"");
    assert_eq!(view.extent, Extent::AutoLonLat);
}

/// **The three other spellings are refused on a projected view**, each naming the one to write.
///
/// `min`/`max` and `x`/`y` state a frame in the space the projection *produces*, which is the
/// output of a calculation nobody should do by hand; `margin` is headroom the outward snap already
/// supplies, and a margin inside an aligned square would only shrink the frame away from the
/// alignment it exists to have.
#[test]
fn the_unprojected_extent_spellings_are_refused_on_a_projected_view() {
    let refused = |extent: &str| {
        err(&projected(&format!(
            "projection = \"web_mercator\"\n{extent}"
        )))
    };

    let message = refused("extent = { min = -25.0, max = 25.0 }");
    assert!(message.contains("names min, max"), "{message}");
    assert!(message.contains("lon = ["), "{message}");

    let message = refused("extent = { x = [-18, 19], y = [-22, 24] }");
    assert!(message.contains("names x, y"), "{message}");
    assert!(message.contains("lon = ["), "{message}");

    let message = refused("extent = { auto = true, margin = 0.25 }");
    assert!(message.contains("outward snap"), "{message}");
    assert!(message.contains("extent = \"auto\""), "{message}");

    // Half a box is still half a box.
    let message = refused("extent = { lon = [-8.6, 1.8] }");
    assert!(message.contains("`lon` without `lat`"), "{message}");
}

/// The reverse: a degree box on a view with nothing to transform it, which would quantise two
/// degrees as though they were the file's own units.
#[test]
fn a_degree_box_is_refused_on_an_unprojected_view() {
    let message = err(&projected(
        "extent = { lon = [-8.6, 1.8], lat = [49.9, 60.9] }",
    ));
    assert!(message.contains("no projection"), "{message}");
    assert!(message.contains("web_mercator"), "{message}");
}

/// **A value outside ±180 or ±90 is not a coordinate** (§2), on either axis.
#[test]
fn a_frame_outside_the_wgs84_range_is_refused() {
    let refused = |extent: &str| {
        err(&projected(&format!(
            "projection = \"web_mercator\"\n{extent}"
        )))
    };

    let message = refused("extent = { lon = [-190.0, 1.8], lat = [49.9, 60.9] }");
    assert!(message.contains("not a longitude"), "{message}");
    assert!(message.contains("WGS84"), "{message}");

    let message = refused("extent = { lon = [-8.6, 1.8], lat = [49.9, 95.0] }");
    assert!(message.contains("not a latitude"), "{message}");
}

/// **A box crossing the antimeridian is refused**, naming the wider box that does not cross: a
/// frame is one aligned square and an aligned square does not wrap.
#[test]
fn a_box_crossing_the_antimeridian_is_refused() {
    let message = err(&projected(
        "projection = \"web_mercator\"\nextent = { lon = [170.0, -170.0], lat = [-10.0, 10.0] }",
    ));
    assert!(message.contains("antimeridian"), "{message}");
    assert!(message.contains("lon = [-170, 170]"), "{message}");

    // Latitude cannot wrap at all, so an inverted one is only ever inverted.
    let message = err(&projected(
        "projection = \"web_mercator\"\nextent = { lon = [-8.6, 1.8], lat = [60.9, 49.9] }",
    ));
    assert!(
        message.contains("runs south from its own maximum"),
        "{message}"
    );
    assert!(message.contains("lat = [49.9, 60.9]"), "{message}");
}

/// **A projected view spells its coordinate columns `lon` and `lat`** (§2), and the resolved map
/// puts them on the canonical axes so every reader below sees one coordinate pair.
#[test]
fn a_projected_view_reads_lon_and_lat() {
    let base = "projection = \"web_mercator\"\nextent = { lon = [-180, 180], lat = [-85, 85] }";

    // With no `fields` map at all, the columns are `lon` and `lat` — which is the whole of what
    // makes those the projected view's names.
    let view = view_of(base);
    assert_eq!(view.fields.of("x"), "lon");
    assert_eq!(view.fields.of("y"), "lat");

    // A map moves them, under the geographic names.
    let view = view_of(&format!(
        "{base}\nsource = \"other\"\nfields = {{ lon = \"longitude\", lat = \"latitude\" }}"
    ));
    assert_eq!(view.fields.of("x"), "longitude");
    assert_eq!(view.fields.of("y"), "latitude");

    // `x`/`y` is refused there, naming the geographic spelling: a corpus built with the two
    // exchanged is silently mirrored about the diagonal.
    let message = err(&projected(&format!(
        "{base}\nsource = \"other\"\nfields = {{ x = \"a\", y = \"b\" }}"
    )));
    assert!(
        message.contains("`fields.x` on a projected view"),
        "{message}"
    );
    assert!(message.contains("`fields.lon`"), "{message}");

    // And the reverse, on a view with no projection to read a degree with.
    let message = err(&projected(
        "extent = \"auto\"\nsource = \"other\"\nfields = { lon = \"a\", lat = \"b\" }",
    ));
    assert!(
        message.contains("`fields.lon` on a view that declares no projection"),
        "{message}"
    );
    assert!(message.contains("`fields.x`"), "{message}");

    // A Morton code is a position already placed, so there is no longitude to transform.
    let message = err(&projected(&format!(
        "{base}\nsource = \"other\"\nfields = {{ morton = \"m\" }}"
    )));
    assert!(
        message.contains("`fields.morton` on a projected view"),
        "{message}"
    );
}

/// **The snap, against hand-computed squares.** Each frame below is arithmetic a reader can redo:
/// `x = (lon + 180)/360` under either projection, and `y = 0.5 - lat/180` under equirectangular.
///
/// * `lon [-180, 180], lat [-85.0511287798066, 85.0511287798066]` under `web_mercator` is the unit
///   square exactly — the projection's own domain — and only the whole world holds it.
/// * `lon [-180, -90], lat [45, 90]` under `equirectangular` is `x [0, 0.25], y [0, 0.25]`, whose
///   maxima sit **on** the z2 boundary and so belong to the next tile: it spans two and snaps to
///   z1 (0, 0). The frame must contain the box, which is what decides this.
/// * `lon [-144, -36], lat [-72, -18]` is `x [0.1, 0.4], y [0.6, 0.9]` — strictly inside z1 (0, 1)
///   and straddling the z2 boundary at x = 0.25, so z1 (0, 1).
/// * A single point takes the offset cap and is reported **floored** rather than fitted (§4.2).
#[test]
fn a_stated_box_snaps_to_the_smallest_containing_square() {
    let snap = |projection: &str, extent: &str| {
        let view = view_of(&format!("projection = \"{projection}\"\n{extent}"));
        let Extent::LonLat(asked) = view.extent else {
            panic!("{extent} did not compile to a longitude/latitude box");
        };
        snap_lon_lat(view.projection, &asked)
    };

    let world = snap(
        "web_mercator",
        "extent = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }",
    );
    assert_eq!(world.square, tessera_spatial::AlignedSquare::WORLD);
    assert!(!world.floored);
    assert_eq!(
        world.square.bounds(),
        Bounds {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0
        }
    );

    let boundary = snap(
        "equirectangular",
        "extent = { lon = [-180.0, -90.0], lat = [45.0, 90.0] }",
    );
    assert_eq!(
        boundary.square,
        tessera_spatial::AlignedSquare { z: 1, x: 0, y: 0 }
    );
    assert!(!boundary.floored);

    let inside = snap(
        "equirectangular",
        "extent = { lon = [-144.0, -36.0], lat = [-72.0, -18.0] }",
    );
    assert_eq!(
        inside.square,
        tessera_spatial::AlignedSquare { z: 1, x: 0, y: 1 }
    );

    // A single point: contained in aligned squares at every offset, so it takes the cap of 16 and
    // says the frame was floored. `x = (0 + 180)/360 = 0.5` and `y = 0.5 - 0/180 = 0.5`, so the
    // square is 32768 on each axis.
    let point = snap(
        "equirectangular",
        "extent = { lon = [0.0, 0.0], lat = [0.0, 0.0] }",
    );
    assert_eq!(
        point.square,
        tessera_spatial::AlignedSquare {
            z: 16,
            x: 32768,
            y: 32768
        }
    );
    assert!(point.floored);
}

/// **`tessera check` prints the frame and the snap for a stated box without opening a data file**
/// (`projections.md` §8), and says it cannot under `auto`.
///
/// The declaration here names no points source at all, which is legal, so nothing readable exists:
/// the frame still comes out, because a stated box's square is a function of the projection and the
/// box alone.
#[test]
fn a_check_answers_a_projected_views_frame_from_the_declaration_alone() {
    let stated = parse_str(&projected(
        "projection = \"equirectangular\"\nextent = { lon = [-180.0, -90.0], lat = [45.0, 90.0] }",
    ))
    .expect("the declaration parses");
    let report = crate::check::check(&stated);
    assert_eq!(report.frames.len(), 1);
    assert_eq!(report.frames[0].view, "s0");
    assert_eq!(report.frames[0].projection, "equirectangular");
    let (asked, snap) = report.frames[0].snapped.expect("a stated box snaps");
    assert_eq!(asked.lon_min, -180.0);
    // `x [0, 0.25], y [0, 0.25]`, whose maxima are the z2 boundary and so belong to the next tile.
    assert_eq!(
        snap.square,
        tessera_spatial::AlignedSquare { z: 1, x: 0, y: 0 }
    );
    assert!(!snap.floored);

    let fitted = parse_str(&projected(
        "projection = \"web_mercator\"\nextent = \"auto\"",
    ))
    .expect("the declaration parses");
    let report = crate::check::check(&fitted);
    assert_eq!(report.frames.len(), 1);
    assert!(
        report.frames[0].snapped.is_none(),
        "under `auto` the frame is a function of the data and a check cannot answer it"
    );

    // An unprojected view has no projected frame to preview, and does not appear.
    let plain = parse_str(SEVERITY).expect("the declaration parses");
    assert!(crate::check::check(&plain).frames.is_empty());
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
    assert!(message.contains("`visibility` is `inherited`"), "{message}");

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

/// The content-level key of the same name is gone (decision 0135): a deleted member withdraws
/// supplied content at the fold, and a `[layer.content]` block written for the old modes is
/// refused as carrying an unknown key rather than read with the key ignored (decision 0048).
#[test]
fn content_withdrawal_is_no_longer_a_key() {
    for value in ["true", "false"] {
        let text = with_layer("").replace(
            "  computed = [\"centroid\", \"box\"]",
            &format!("  computed = [\"centroid\"]\n  withdraw_on_member_deletion = {value}"),
        );
        let message = err(&text);
        assert!(
            message.contains("withdraw_on_member_deletion"),
            "the refusal names the key: {message}"
        );
    }
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

    // **An attribute layer's form follows from its membership, so every pin is refused** —
    // refused here, by the same `validate` the online registration calls. A spatial layer's
    // membership is a per-row source once the flush resolves it, so a pin on it holds.
    for word in ServingLayout::PIN_VOCABULARY {
        let spatial = predicate_fixture().replace(
            "membership                = \"enumerated\"\n",
            &format!(
                "membership                = \"spatial\"\nlayout                    = \"{word}\"\n"
            ),
        );
        assert_eq!(
            parse_str(&spatial).unwrap().layers[0].layout,
            ServingLayout::parse_pin(word),
            "layout = \"{word}\" on a spatial layer"
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

/// `[layer.shape]` — what kind of shape a spatial layer's artifacts carry, and nothing else.
#[test]
fn a_spatial_layer_declares_its_shape_kind_and_no_depth() {
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

    for (word, kind) in [
        ("bbox", ShapeKind::Bbox),
        ("circle", ShapeKind::Circle),
        ("ellipse", ShapeKind::Ellipse),
        ("polygon", ShapeKind::Polygon),
    ] {
        let declared = spatial(&format!("\n  [layer.shape]\n  kind = \"{word}\"\n"));
        assert_eq!(
            parse_str(&declared).unwrap().layers[0].shape,
            Some(ShapeDeclaration { kind })
        );
    }

    // **`depth` is deleted** (`polygon-membership.md` §6.1, ruling (g)): every kind is exact, so
    // one written is refused naming where it went rather than as an unknown key.
    let depth = spatial("\n  [layer.shape]\n  kind = \"bbox\"\n  depth = 6\n");
    assert!(
        err(&depth).contains("polygon-membership.md") && err(&depth).contains("depth"),
        "{}",
        err(&depth)
    );
    // Four kinds, so the word is a choice and not a courtesy: absent is refused naming them.
    let absent = spatial("\n  [layer.shape]\n");
    assert!(
        err(&absent).contains("bbox, circle, ellipse, polygon"),
        "{}",
        err(&absent)
    );
    let unknown = spatial("\n  [layer.shape]\n  kind = \"radius\"\n");
    assert!(
        err(&unknown).contains("is not a shape kind"),
        "{}",
        err(&unknown)
    );

    // A shape beside a membership that reads none is a rule nothing evaluates.
    let enumerated = format!(
        "{}\n  [layer.shape]\n  kind = \"bbox\"\n",
        predicate_fixture()
    );
    assert!(
        err(&enumerated).contains("is a rule nothing evaluates"),
        "{}",
        err(&enumerated)
    );

    // **The space lives with the submission** (§4.3): `default_space` on the layer and `space` on
    // a row. `wgs84` asks the view to project, so it is honourable only where the view declares a
    // projection — and this fixture's view declares none, which is a refusal naming that
    // (`projections.md` §5.3).
    let with_space = |word: &str, body: &str| {
        spatial(body).replace(
            "membership                = \"spatial\"",
            &format!(
                "default_space             = \"{word}\"\nmembership                = \"spatial\""
            ),
        )
    };
    let view = with_space("view", "\n  [layer.shape]\n  kind = \"bbox\"\n");
    assert!(parse_str(&view).is_ok(), "{}", err(&view));
    let wgs84 = with_space("wgs84", "\n  [layer.shape]\n  kind = \"bbox\"\n");
    assert!(err(&wgs84).contains("projections.md"), "{}", err(&wgs84));
    let spaceless = with_space("view", "");
    assert!(
        err(&spaceless).contains("neither `[layer.shape]` nor an authored shape content"),
        "{}",
        err(&spaceless)
    );

    // **An authored shape content is geometry a space governs too** (`polygon-membership.md`
    // §6.1): it is read in the space its row declares exactly as a membership shape is, so a
    // layer that declares one and no `[layer.shape]` may still name the space its drawings are
    // written in — and is refused for the same unhonourable pair.
    let authored = |word: &str| {
        format!(
            "{}\n  [[layer.content.supplied]]\n  name = \"outline\"\n  type = \"polygon\"\n  \
             require_member_visibility = \"inherited\"\n",
            predicate_fixture().replace(
                "membership                = \"enumerated\"",
                &format!(
                    "default_space             = \"{word}\"\nmembership                = \
                     \"enumerated\""
                ),
            )
        )
    };
    let drawn = authored("view");
    assert!(parse_str(&drawn).is_ok(), "{}", err(&drawn));
    let drawn_wgs84 = authored("wgs84");
    assert!(
        err(&drawn_wgs84).contains("projections.md"),
        "{}",
        err(&drawn_wgs84)
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
    assert!(message.contains("give one"), "{message}");
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
    assert!(bound_err(&text, &[]).contains("entity_id"));

    let text = with_line(SEVERITY, "field = \"  \"");
    assert!(err(&text).contains("`field` is empty"));
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
        config.views[0].point_visibility.default.as_deref(),
        Some("ir:analyst,ir:legal"),
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
    assert!(message.contains("`visibility` is `inherited`"), "{message}");
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
    let acquired = config.acquire().expect("the entity-space inputs acquire");
    let registry = config.build_views().expect("the registry compiles");
    let view = crate::config::acquire_view(&registry[0]).expect("the view is declared");
    assert_eq!(view.points, dir.path().join("geometry.parquet"));
    assert!(
        matches!(&view.access.source, crate::config::AccessSource::Relation(p) if *p == dir.path().join("pairs.parquet")),
        "{:?}",
        view.access
    );
    assert_eq!(
        acquired.attribute_sources[0].path,
        dir.path().join("corpus.parquet")
    );
    assert!(acquired.layers.is_empty());
    assert_eq!(
        registry[0].extent,
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
    let acquired = config.acquire().unwrap();
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
    let acquired = config.acquire().unwrap();
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
    let acquired = config.acquire().expect("both sources acquire");
    let paths: Vec<Option<PathBuf>> = acquired.layers.iter().map(artifact_path).collect();
    assert_eq!(
        paths,
        vec![
            Some(dir.path().join("hdbscan.parquet")),
            Some(dir.path().join("other.parquet"))
        ]
    );
}

/// The anchor view is a declaration, not a default (decision 0112), so one naming a view the
/// registry does not carry refuses listing the candidates.
#[test]
fn a_build_refuses_an_anchor_the_config_does_not_declare() {
    let config = parse_bound(ACQUIRED, &HashMap::new()).unwrap();
    let registry = config.build_views().unwrap();
    let mut config = config;
    config.allocation_view = Some("s9".to_string());
    let message = format!(
        "{}",
        config
            .anchor_view(&registry)
            .expect_err("expected a refusal")
    );
    assert!(message.contains("allocation_view"), "{message}");
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
    let registry = config.build_views().unwrap();
    let message = format!(
        "{}",
        crate::config::acquire_view(&registry[0]).expect_err("expected a refusal")
    );
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
    let registry = config.build_views().unwrap();
    let acquired = crate::config::acquire_view(&registry[0]).expect("a field route acquires");
    assert!(
        matches!(&acquired.access.source, AccessSource::Field(f) if f == "categories"),
        "{:?}",
        acquired.access
    );
    assert_eq!(acquired.access.default.as_deref(), Some("public"));

    let only_default = ACQUIRED.replace(
        "{ source = \"pairs\", default = \"public\" }",
        "{ default = \"ir:analyst\" }",
    );
    let config = parse_bound(&only_default, &HashMap::new()).unwrap();
    let registry = config.build_views().unwrap();
    let acquired = crate::config::acquire_view(&registry[0]).expect("a default alone acquires");
    assert!(
        matches!(acquired.access.source, AccessSource::Default),
        "{:?}",
        acquired.access
    );
    assert_eq!(acquired.access.default.as_deref(), Some("ir:analyst"));
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
    let message = format!("{}", config.acquire().expect_err("expected a refusal"));
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
    assert!(message.contains("given twice"), "{message}");
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

// ---------------------------------------------------------------------------------------------
// View groups (`views.md` §3), their rosters, their metadata and the scopes that name them
// ---------------------------------------------------------------------------------------------

/// One group over [`SEVERITY`]'s sources, form A: two views, one file each, two typed metadata
/// names. Appended to `SEVERITY`, which declares the plain view `s0` beside it.
const GROUP: &str = r#"
[[view_group]]
name             = "quarter"
title            = "By quarter"
extent           = { min = -40.0, max = 40.0 }
visibility       = "public"
point_visibility = { field = "categories", default = "public" }
metadata         = { label = "text", starts = "timestamp_us" }

[[view_group.view]]
key    = "2026-Q2"
source = "topics"
label  = "Q2 2026"
starts = 2026-04-01T00:00:00Z

[[view_group.view]]
key    = "2026-Q3"
source = "other"
label  = "Q3 2026"
starts = 2026-07-01T00:00:00Z
"#;

/// The same group written in form B: one points file behind a discriminator, and the roster as a
/// table of its own.
const GROUP_B: &str = r#"
[[view_group]]
name             = "quarter"
extent           = { min = -40.0, max = 40.0 }
source           = "topics"
fields           = { view = "quarter" }
point_visibility = { field = "categories", default = "public" }
metadata         = { label = "text" }

[view_group.views]
source = "other"
fields = { key = "quarter" }
"#;

fn with_group(extra: &str) -> String {
    format!("{SEVERITY}{GROUP}{extra}")
}

#[test]
fn a_group_compiles_its_roster_its_metadata_and_its_shared_settings() {
    let config = parse_str(&with_group("")).expect("the group parses");
    assert_eq!(config.views.len(), 1, "a group is not a view");
    let group = &config.view_groups[0];
    assert_eq!(group.name, "quarter");
    assert_eq!(group.source, None, "form A declares no group-level source");
    assert_eq!(group.point_visibility.default.as_deref(), Some("public"));
    assert_eq!(
        group
            .metadata
            .iter()
            .map(|m| (m.name.as_str(), m.ty.arrow_type_name()))
            .collect::<Vec<_>>(),
        [("label", "text"), ("starts", "timestamp_us")]
    );
    assert_eq!(group.declared_keys(), ["2026-Q2", "2026-Q3"]);
    let Roster::Inline(views) = &group.roster else {
        panic!("form A compiles to an inline roster");
    };
    assert_eq!(
        views[0].metadata.get("label"),
        Some(&MetadataValue::Text("Q2 2026".to_string()))
    );
    // 2026-04-01T00:00:00Z, in the one unit a `timestamp_us` may hold.
    assert_eq!(
        views[0].metadata.get("starts"),
        Some(&MetadataValue::TimestampUs(1_775_001_600_000_000))
    );
    assert!(views[0].source.is_some(), "each view names its own points");
}

#[test]
fn a_group_compiles_the_roster_table_form() {
    let config = parse_str(&format!("{SEVERITY}{GROUP_B}")).expect("form B parses");
    let group = &config.view_groups[0];
    assert!(group.source.is_some(), "form B's points are the group's");
    assert_eq!(group.fields.of("view"), "quarter", "the discriminator");
    let Roster::Table(table) = &group.roster else {
        panic!("form B compiles to a roster table");
    };
    assert_eq!(table.fields.of("key"), "quarter");
    assert!(group.declared_keys().is_empty(), "the keys are in the file");
}

/// **The roster is declared once.** The two forms say different things about where the points
/// are, so a group writing both has said they are in two places.
#[test]
fn declaring_both_roster_forms_is_refused() {
    let text = format!("{SEVERITY}{GROUP}\n[view_group.views]\nsource = \"other\"\n");
    let message = err(&text);
    assert!(message.contains("both roster forms"), "{message}");
    assert!(message.contains("Write one of them"), "{message}");
}

/// Neither form and no `source`: the group's points come from nowhere, and `[defaults].source`
/// deliberately does not reach a group.
#[test]
fn a_group_with_no_roster_and_no_source_is_refused() {
    let text = format!(
        "{SEVERITY}\n[[view_group]]\nname = \"quarter\"\nextent = \"auto\"\n\
         point_visibility = {{ default = \"public\" }}\n"
    );
    let message = err(&text);
    assert!(message.contains("come from nowhere"), "{message}");
    assert!(message.contains("`[defaults].source`"), "{message}");
}

/// Form B is two files, and the group needs both: the roster lists the views, and the group's own
/// `source` holds their points behind the discriminator.
#[test]
fn a_roster_table_without_the_groups_points_is_refused() {
    let text = format!("{SEVERITY}{GROUP_B}").replace("source           = \"topics\"\n", "");
    let message = err(&text);
    assert!(
        message.contains("a `[view_group.views]` roster and no `source`"),
        "{message}"
    );

    // And the roster table's own file, which is not the group's.
    let text = format!("{SEVERITY}{GROUP_B}").replace(
        "[view_group.views]\nsource = \"other\"\n",
        "[view_group.views]\n",
    );
    assert!(
        err(&text).contains("`source` is required"),
        "{}",
        err(&text)
    );
}

/// **Under form A the file is the view**, so a group-level `source` beside inline blocks is a
/// second place the points could be.
#[test]
fn a_group_level_source_beside_inline_views_is_refused() {
    let text = with_group("").replace(
        "name             = \"quarter\"",
        "name             = \"quarter\"\nsource           = \"topics\"",
    );
    let message = err(&text);
    assert!(
        message.contains("`source` beside `[[view_group.view]]` blocks"),
        "{message}"
    );
}

/// A group declaring neither roster form mints its views from the discriminator, and there is no
/// roster record for a metadata value to sit on.
#[test]
fn metadata_with_no_roster_is_refused() {
    let text = format!(
        "{SEVERITY}\n[[view_group]]\nname = \"quarter\"\nextent = \"auto\"\nsource = \"topics\"\n\
         metadata = {{ label = \"text\" }}\npoint_visibility = {{ default = \"public\" }}\n"
    );
    let message = err(&text);
    assert!(message.contains("`metadata` with no roster"), "{message}");
}

/// **Chains are refused, so the owner of a key set is always one hop away** (§3.3).
#[test]
fn a_members_chain_is_refused() {
    let text = with_group(
        "\n[[view_group]]\nname = \"quarter_map\"\nmembers = \"quarter\"\nextent = \"auto\"\n\
         source = \"other\"\npoint_visibility = { default = \"public\" }\n\
         \n[[view_group]]\nname = \"quarter_third\"\nmembers = \"quarter_map\"\nextent = \"auto\"\n\
         source = \"topics\"\npoint_visibility = { default = \"public\" }\n",
    );
    let message = err(&text);
    assert!(message.contains("chains are refused"), "{message}");
    assert!(
        message.contains("Name 'quarter' here instead"),
        "the refusal must name the owner: {message}"
    );
}

#[test]
fn a_members_group_takes_the_owners_views_and_declares_no_roster() {
    let sharing = "\n[[view_group]]\nname = \"quarter_map\"\nmembers = \"quarter\"\n\
                   extent = \"auto\"\nsource = \"other\"\nfields = { view = \"quarter\" }\n\
                   point_visibility = { default = \"public\" }\n";
    let config = parse_str(&with_group(sharing)).expect("a members group parses");
    let shared = &config.view_groups[1];
    assert_eq!(shared.members.as_deref(), Some("quarter"));
    assert!(matches!(shared.roster, Roster::Discriminator));
    assert!(shared.metadata.is_empty());

    // Keys, ordinals and metadata belong to the group that owns the views.
    let message = err(&with_group(&sharing.replace(
        "extent = \"auto\"",
        "extent = \"auto\"\nmetadata = { label = \"text\" }",
    )));
    assert!(message.contains("`metadata` on a group"), "{message}");
    assert!(message.contains("Declare it on 'quarter'"), "{message}");

    let message = err(&with_group(&format!(
        "{sharing}\n[[view_group.view]]\nkey = \"2026-Q2\"\nsource = \"topics\"\n"
    )));
    assert!(message.contains("a roster on a group"), "{message}");
}

/// **A title is the layout's, not the key set's** (`views.md` §3.3, contracts §3.2 r61). Two
/// groups over one roster are two layouts, and the group descriptor's `title` is taken from the
/// group that declared it — so a `members` group with a title of its own publishes that one, and a
/// `members` group with none publishes **none** rather than inheriting the owner's. The second
/// half is the one worth pinning: inheriting would put the owner's name on a second layout that a
/// principal may reach without reaching the owner at all.
#[test]
fn a_members_groups_title_is_its_own_and_is_never_the_owners() {
    let sharing = |title: &str| {
        format!(
            "\n[[view_group]]\nname = \"quarter_map\"\nmembers = \"quarter\"\n{title}\
             extent = \"auto\"\nsource = \"other\"\nfields = {{ view = \"quarter\" }}\n\
             point_visibility = {{ default = \"public\" }}\n"
        )
    };

    let titles = |text: &str| {
        let config = parse_str(text).expect("both groups parse");
        let registry = config.build_views().expect("a form A roster needs no file");
        let resolved = stub_view_args(&registry);
        config
            .group_registry(&registry, &resolved)
            .into_iter()
            .map(|group| (group.name, group.title))
            .collect::<Vec<_>>()
    };

    // The owner declares one (`GROUP`'s own `title`), and the sharing group declares its own.
    assert_eq!(
        titles(&with_group(&sharing("title = \"Quarters on the map\"\n"))),
        [
            ("quarter".to_string(), Some("By quarter".to_string())),
            (
                "quarter_map".to_string(),
                Some("Quarters on the map".to_string())
            ),
        ]
    );

    // And with none declared it is `None` — served as `null`, not as the owner's title.
    assert_eq!(
        titles(&with_group(&sharing(""))),
        [
            ("quarter".to_string(), Some("By quarter".to_string())),
            ("quarter_map".to_string(), None),
        ]
    );
}

/// The two fields [`Config::group_registry`] reads off a resolved view — its id and its frame —
/// with the rest of [`crate::ViewArgs`] filled in inertly. A test of the registry is a test of the
/// declaration's own arithmetic, and building the acquisition half would be building a corpus.
fn stub_view_args(registry: &[BuildView]) -> Vec<crate::ViewArgs> {
    registry
        .iter()
        .map(|view| crate::ViewArgs {
            view_id: view.id.clone(),
            projection: view.projection,
            extent: tessera_spatial::Bounds {
                x_min: -40.0,
                x_max: 40.0,
                y_min: -40.0,
                y_max: 40.0,
            },
            points: PathBuf::new(),
            point_fields: Fields::default(),
            select: None,
            access: AccessInput {
                source: AccessSource::Default,
                default: Some("public".to_string()),
            },
            visibility: view.visibility.clone(),
        })
        .collect()
}

#[test]
fn members_naming_no_group_is_refused() {
    let text = with_group(
        "\n[[view_group]]\nname = \"quarter_map\"\nmembers = \"quarterly\"\nextent = \"auto\"\n\
         source = \"other\"\npoint_visibility = { default = \"public\" }\n",
    );
    let message = err(&text);
    assert!(
        message.contains("names no `[[view_group]]` block"),
        "{message}"
    );
    assert!(message.contains("quarter"), "{message}");
}

/// `:`, `#` and `@` are reserved out of a view name and a key, because each is what makes an id or
/// a pinned filter leaf unambiguous (`views.md` §3.2).
#[test]
fn the_reserved_characters_are_refused_in_a_name_and_in_a_key() {
    let message = err(&with_group("").replace("\"quarter\"", "\"quarter:one\""));
    assert!(message.contains("reserved out of a view name"), "{message}");

    // `#` is no longer reserved by name — it addressed an ordinal and ordinals are gone
    // (decision 0113) — so it falls to the charset like any other punctuation.
    let message = err(&with_group("").replace("\"2026-Q2\"", "\"2026#Q2\""));
    assert!(message.contains("column-name charset"), "{message}");

    let message = err(&with_group("").replace("\"2026-Q2\"", "\"2026.Q2\""));
    assert!(message.contains("column-name charset"), "{message}");

    let message = err(&SEVERITY.replace("name             = \"s0\"", "name             = \"s@0\""));
    assert!(message.contains("reserved out of a view name"), "{message}");
}

#[test]
fn two_views_of_one_group_may_not_share_a_key() {
    let message = err(&with_group("").replace("\"2026-Q3\"", "\"2026-Q2\""));
    assert!(message.contains("declared twice"), "{message}");
}

/// A layer's `views` and a scope's `group` name either kind, so one word for both is ambiguous
/// wherever they meet.
#[test]
fn a_group_and_a_view_may_not_share_a_name() {
    let message = err(&with_group("").replace(
        "name             = \"quarter\"",
        "name             = \"s0\"",
    ));
    assert!(
        message.contains("has the name of a `[[view]]` block"),
        "{message}"
    );

    // And two groups of one name, on the same argument the duplicate-view rule rests on.
    let message = err(&with_group(GROUP));
    assert!(message.contains("is declared twice"), "{message}");
}

/// **Metadata names are bounded by the roster's own keys** (§3.2): the inline block mixes them
/// with the closed set, so a name in both is a key with two readings.
#[test]
fn a_metadata_name_may_not_take_a_roster_key() {
    for reserved in ["key", "source", "visibility"] {
        let text = with_group("").replace("label = \"text\"", &format!("{reserved} = \"text\""));
        err(&text);
    }
    // And the discriminator, where the group carries one.
    let text = format!("{SEVERITY}{GROUP_B}").replace("label = \"text\"", "quarter = \"text\"");
    let message = err(&text);
    assert!(message.contains("discriminator column"), "{message}");
}

/// The three settings a group's views share by definition, refused on a view of it — and named as
/// group-level keys rather than reported as unknown ones.
#[test]
fn a_roster_entry_may_not_declare_a_group_level_key() {
    for (key, value) in [
        ("extent", "\"auto\""),
        ("projection", "\"web_mercator\""),
        ("point_visibility", "{ default = \"public\" }"),
    ] {
        let text = with_group("").replace(
            "key    = \"2026-Q2\"",
            &format!("key    = \"2026-Q2\"\n{key} = {value}"),
        );
        let message = err(&text);
        assert!(message.contains("is a group-level key"), "{key}: {message}");
        assert!(message.contains("Write `"), "{key}: {message}");
    }
}

/// The block cannot be a `deny_unknown_fields` struct — it mixes a closed set with the declared
/// metadata names — so the closure is kept by hand, and this is the assertion that it is kept.
#[test]
fn an_unknown_key_in_a_roster_entry_is_refused_against_the_declared_names() {
    let text = with_group("").replace("key    = \"2026-Q2\"", "key    = \"2026-Q2\"\nnonesuch = 1");
    let message = err(&text);
    assert!(
        message.contains("is not a key of a `[[view_group.view]]` block"),
        "{message}"
    );
    assert!(message.contains("label, starts"), "{message}");
}

/// A roster record is immutable (decision 0108), so a metadata value left out is a view served
/// with that field missing for the whole of its life.
#[test]
fn a_roster_entry_carries_every_declared_metadata_name() {
    let text = with_group("").replace("label  = \"Q2 2026\"\n", "");
    let message = err(&text);
    assert!(message.contains("no `label`"), "{message}");
    assert!(message.contains("immutable"), "{message}");
}

#[test]
fn a_metadata_value_is_typed_against_its_declaration() {
    let text = with_group("").replace("starts = 2026-04-01T00:00:00Z", "starts = \"April\"");
    let message = err(&text);
    assert!(message.contains("timestamp_us"), "{message}");

    let text = with_group("").replace("label = \"text\"", "label = \"u8\"");
    let message = err(&text);
    assert!(message.contains("write an integer"), "{message}");

    let text = with_group("")
        .replace("label = \"text\"", "label = \"u8\"")
        .replace("label  = \"Q2 2026\"", "label  = 900");
    let message = err(&text);
    assert!(message.contains("0 to 255"), "{message}");

    let text = with_group("").replace("starts = \"timestamp_us\"", "starts = \"nonesuch\"");
    let message = err(&text);
    assert!(message.contains("unknown type 'nonesuch'"), "{message}");
}

/// A local date-time names an instant only against a time zone nobody declared.
#[test]
fn a_metadata_timestamp_needs_an_offset() {
    let text = with_group("").replace("2026-04-01T00:00:00Z", "2026-04-01T00:00:00");
    let message = err(&text);
    assert!(
        message.contains("needs a date, a time and an offset"),
        "{message}"
    );
}

/// A group's own gate and a roster record's own are stored where the evaluation reads them: the
/// group's on the group, the record's on its view (`views.md` §6).
#[test]
fn a_groups_gate_and_a_roster_records_gate_are_both_recorded() {
    let text = with_group("").replace(
        "visibility       = \"public\"",
        "visibility       = \"ir:analyst\"",
    );
    let config = ok(&text);
    assert_eq!(
        config.view_groups[0].visibility.as_deref(),
        Some(&["ir:analyst".to_string()][..])
    );

    let text = with_group("").replace(
        "key    = \"2026-Q2\"",
        "key    = \"2026-Q2\"\nvisibility = \"ir:analyst\"",
    );
    let config = ok(&text);
    let gated = config
        .view_groups
        .iter()
        .filter_map(|g| match &g.roster {
            crate::config::Roster::Inline(views) => Some(views),
            _ => None,
        })
        .flatten()
        .filter(|v| v.visibility.as_deref() == Some(&["ir:analyst".to_string()][..]))
        .count();
    assert_eq!(gated, 1, "one roster record carries the gate, and one only");
}

#[test]
fn an_attribute_scopes_to_a_group_that_owns_its_views() {
    let scoped = with_group("").replace(
        "[[attribute]]\nname       = \"severity\"",
        "[[attribute]]\nscope      = { group = \"quarter\" }\nname       = \"severity\"",
    );
    let config = parse_str(&scoped).expect("a scoped attribute parses");
    assert_eq!(config.scopes.attribute("severity"), Some("quarter"));

    let message = err(&scoped.replace("group = \"quarter\"", "group = \"quarterly\""));
    assert!(
        message.contains("names no `[[view_group]]` block"),
        "{message}"
    );

    let message = err(&scoped.replace(
        "scope      = { group = \"quarter\" }",
        "scope      = \"quarterly\"",
    ));
    assert!(message.contains("not a value this key takes"), "{message}");
}

/// **The group named is the one that owns the members** (§5): a scope on a `members` group would
/// be a second name for one column family.
#[test]
fn a_scope_naming_a_members_group_points_at_the_owner() {
    let text = with_group(
        "\n[[view_group]]\nname = \"quarter_map\"\nmembers = \"quarter\"\nextent = \"auto\"\n\
         source = \"other\"\nfields = { view = \"quarter\" }\n\
         point_visibility = { default = \"public\" }\n",
    )
    .replace(
        "[[attribute]]\nname       = \"severity\"",
        "[[attribute]]\nscope      = { group = \"quarter_map\" }\nname       = \"severity\"",
    );
    let message = err(&text);
    assert!(
        message.contains("declares `members = \"quarter\"`"),
        "{message}"
    );
    assert!(
        message.contains("scope = { group = \"quarter\" }"),
        "the refusal must point at the owner: {message}"
    );
}

/// **A scoped attribute may declare its own `source`, and that file carries the discriminator**
/// (`views.md` §5): one row per `(entity, view)`, `fields.view` saying which view each row's value
/// is for. Without it the file would be read as entity space, which would take one arbitrary
/// view's values as every view's.
#[test]
fn a_scoped_attribute_may_read_its_own_source_through_fields_view() {
    let text = with_group(
        "\n[[attribute]]\nname = \"sentiment\"\ntype = \"f32\"\n\
         scope = { group = \"quarter\" }\nindex = true\nsource = \"other\"\n\
         fields = { view = \"quarter\" }\nentity_id_field = \"doc\"\n",
    );
    let config = ok(&text);
    let scoped = &config.scoped_attributes[0];
    assert_eq!(scoped.group, "quarter");
    let source = scoped.source.as_ref().expect("its own source is recorded");
    assert_eq!(source.view_field, "quarter");
    assert_eq!(source.entity_id, "doc");
    assert!(source.path.ends_with("other.parquet"));
    assert!(
        !config
            .schema
            .attributes
            .iter()
            .any(|a| a.name == "sentiment"),
        "a family is not one of the declared scalars, whatever it reads from"
    );

    // `fields.view` is optional and defaults to `view`, the same default a scoped layer's
    // artifacts source takes — one word, one meaning, across the declaration.
    let defaulted = text.replace("fields = { view = \"quarter\" }\n", "");
    assert_eq!(
        ok(&defaulted).scoped_attributes[0]
            .source
            .as_ref()
            .unwrap()
            .view_field,
        "view"
    );
}

/// `fields` names the view discriminator and nothing else, so the two declarations that have
/// nothing to say with it are refused rather than reading it as a default.
#[test]
fn fields_on_an_attribute_that_has_no_view_to_choose_is_refused() {
    let entity_scope = with_group(
        "\n[[attribute]]\nname = \"weight\"\ntype = \"f32\"\nindex = true\n\
         source = \"other\"\nfields = { view = \"quarter\" }\n",
    );
    assert!(
        err(&entity_scope).contains("group-scoped"),
        "{}",
        err(&entity_scope)
    );

    let no_source = with_group(
        "\n[[attribute]]\nname = \"sentiment\"\ntype = \"f32\"\n\
         scope = { group = \"quarter\" }\nindex = true\nfields = { view = \"quarter\" }\n",
    );
    assert!(
        err(&no_source).contains("there is no `source` here"),
        "{}",
        err(&no_source)
    );

    let stray = with_group(
        "\n[[attribute]]\nname = \"sentiment\"\ntype = \"f32\"\n\
         scope = { group = \"quarter\" }\nindex = true\nsource = \"other\"\n\
         fields = { entity_id = \"doc\" }\n",
    );
    assert!(
        err(&stray).contains("`fields.entity_id` is not a field"),
        "{}",
        err(&stray)
    );
}

/// **A scoped `text` column must be indexed**: the record blob is one bundle-wide list addressed
/// by a column's position in it, and a family has no position — so the token index is the only
/// home its prose has, and a declaration without one stores nothing at all.
#[test]
fn a_scoped_text_column_without_an_index_is_refused() {
    let text = with_group(
        "\n[[attribute]]\nname = \"note\"\ntype = \"text\"\n\
         scope = { group = \"quarter\" }\n",
    );
    let message = err(&text);
    assert!(message.contains("`index = true`"), "{message}");
}

/// A layer may be drawn on a whole group, and a **scoped** layer only on that group's views.
#[test]
fn a_layer_may_name_a_group_and_a_scoped_one_may_name_only_its_own() {
    let over_group = with_group(&LAYER.replace(
        "views                     = [\"s0\"]",
        "views                     = [\"quarter\"]",
    ));
    let config = parse_str(&over_group).expect("a layer over a group parses");
    assert_eq!(config.layers[0].views, ["quarter"]);
    assert_eq!(config.scopes.layer("clusters/a"), None);

    let scoped = over_group.replace(
        "views                     = [\"quarter\"]",
        "views                     = [\"quarter\"]\nscope                     = { group = \"quarter\" }",
    );
    let config = parse_str(&scoped).expect("a scoped layer parses");
    assert_eq!(config.scopes.layer("clusters/a"), Some("quarter"));

    let message = err(&scoped.replace(
        "views                     = [\"quarter\"]\nscope",
        "views                     = [\"quarter\", \"s0\"]\nscope",
    ));
    assert!(message.contains("Nameable here: quarter"), "{message}");

    let message = err(&over_group.replace(
        "views                     = [\"quarter\"]",
        "views                     = [\"nonesuch\"]",
    ));
    assert!(
        message.contains("no `[[view]]` or `[[view_group]]` block declares"),
        "{message}"
    );
    assert!(message.contains("quarter"), "{message}");
}

/// `fields.view` says where a scoped layer's discriminator is, and an unscoped layer has none —
/// the map says *where*, never *whether* (`configuration.md` §8).
#[test]
fn fields_view_on_an_unscoped_layer_is_refused() {
    let text = with_group(&LAYER.replace(
        "views                     = [\"s0\"]",
        "views                     = [\"s0\"]\nsource                    = \"hdbscan\"\n\
         fields                    = { view = \"quarter\" }",
    ));
    let message = err(&text);
    assert!(
        message.contains("names a field this object never declared"),
        "{message}"
    );
    assert!(message.contains("entity-scoped"), "{message}");
}

/// ⊘ The multi-view build is specified and not implemented (`views.md` §7), so a declaration
/// carrying a group refuses at the build rather than materialising its plain views alone.
#[test]
fn a_declaration_with_a_group_materialises_the_groups_views_too() {
    let config = parse_str(&with_group("")).expect("the group parses");
    let registry = config.build_views().expect("the registry compiles");
    assert!(
        registry.iter().any(|v| v.id.contains(':')),
        "a group's views are `group:key`: {:?}",
        registry.iter().map(|v| v.id.as_str()).collect::<Vec<_>>()
    );
    // With several views the anchor is a declaration rather than a default (decision 0112).
    let message = format!(
        "{}",
        config
            .anchor_view(&registry)
            .expect_err("several views and no anchor")
    );
    assert!(message.contains("allocation_view"), "{message}");
    // One view, and naming it is noise.
    let one = parse_str(SEVERITY).unwrap();
    let registry = one.build_views().unwrap();
    assert_eq!(registry.len(), 1);
    assert_eq!(registry[one.anchor_view(&registry).unwrap()].id, "s0");
}

/// **The fixture is the acceptance case**, read from the repository rather than copied: it is
/// `test_corpora/multiview/`'s own `corpus.toml`, written against `views.md` r6 before any of this
/// existed, and its README's feature table is the checklist this stage is measured against.
#[test]
fn the_multiview_fixture_parses() {
    let declared =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_corpora/multiview/corpus.toml");
    let text = std::fs::read_to_string(&declared).expect("the fixture is in the repository");
    // **Written beside a vocabulary file rather than parsed in place.** A closed `[[vocabulary]]`
    // naming a source is *read* at parse — the values are part of the declaration — and the
    // fixture's parquets are derived data `prepare.py` writes, not committed beside it. So the
    // declaration is the repository's, byte for byte, and the one file this parse opens is a
    // minimal stand-in for `kind`'s nine keys.
    let dir = tempfile::tempdir().expect("tempdir");
    write_keys(&dir.path().join("vocab-kind.parquet"), &["A", "P"]);
    let path = dir.path().join("corpus.toml");
    std::fs::write(&path, text).expect("write the fixture's declaration");
    let config = Config::parse(&path, &HashMap::new()).expect("the multiview fixture parses");
    assert_eq!(
        config
            .views
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        ["world", "world_flat"]
    );
    assert_eq!(
        config
            .view_groups
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        ["quarter", "quarter_alt"]
    );
    assert_eq!(
        config.view_groups[0].declared_keys(),
        ["2026-Q1", "2026-Q2", "2026-Q3", "2026-Q4"]
    );
    assert_eq!(config.view_groups[1].members.as_deref(), Some("quarter"));
    assert_eq!(config.view_groups[1].fields.of("view"), "quarter");
    assert_eq!(config.scopes.attribute("sentiment"), Some("quarter"));
    assert_eq!(config.scopes.layer("quarter_clusters"), Some("quarter"));
    assert_eq!(config.scopes.layer("collections"), None);
    // **The shape layer spans two frames** (decision 0111): its two views place their points by
    // different functions, which the pre-0111 parse refused outright.
    let regions = config
        .layers
        .iter()
        .find(|l| l.name == "regions")
        .expect("the shape layer");
    assert_eq!(regions.views, ["world", "world_flat"]);
    assert_eq!(
        config
            .views
            .iter()
            .filter(|v| regions.views.contains(&v.name))
            .map(|v| v.projection.name())
            .collect::<Vec<_>>(),
        ["web_mercator", "equirectangular"]
    );
    // The group-scoped attribute names no source of its own, so it is read from each view's own
    // points file rather than from `[defaults].source` (`views.md` §5).
    assert!(
        !config
            .attribute_sources
            .iter()
            .flat_map(|s| s.attributes.iter())
            .any(|&i| config.schema.attributes[i].name == "sentiment"),
        "a scoped attribute with no source does not take `[defaults].source`"
    );
}

/// A one-column vocabulary file: `key`, and the codes assigned in the order given.
fn write_keys(path: &Path, keys: &[&str]) {
    let schema = std::sync::Arc::new(arrow::datatypes::Schema::new(vec![
        arrow::datatypes::Field::new("key", arrow::datatypes::DataType::Utf8, false),
    ]));
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema.clone(),
        vec![std::sync::Arc::new(arrow::array::StringArray::from(
            keys.to_vec(),
        ))],
    )
    .expect("one column");
    let file = std::fs::File::create(path).expect("create the vocabulary file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, schema, None).expect("open the writer");
    writer.write(&batch).expect("write");
    writer.close().expect("close");
}

// ---------------------------------------------------------------------------------------------
// The roster minted from the discriminator (`views.md` §3.1's third form)
// ---------------------------------------------------------------------------------------------

/// A points file behind a discriminator: `entity_id`, `quarter`, and nothing else this parse
/// reads. The `quarter` column is whatever `discriminator` holds, so a case can write a null or
/// another type into it as easily as a key.
fn write_discriminator(path: &Path, discriminator: arrow::array::ArrayRef, nullable: bool) {
    let schema = std::sync::Arc::new(arrow::datatypes::Schema::new(vec![
        arrow::datatypes::Field::new("entity_id", arrow::datatypes::DataType::UInt64, false),
        arrow::datatypes::Field::new("quarter", discriminator.data_type().clone(), nullable),
    ]));
    let ids: Vec<u64> = (0..discriminator.len() as u64).collect();
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(arrow::array::UInt64Array::from(ids)),
            discriminator,
        ],
    )
    .expect("two columns");
    let file = std::fs::File::create(path).expect("create the points file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, schema, None).expect("open the writer");
    writer.write(&batch).expect("write");
    writer.close().expect("close");
}

/// The ordinary case: one row per key, in the order given, so a case can assert the mint does
/// **not** take file order.
fn write_discriminated(path: &Path, keys: &[&str]) {
    write_discriminator(
        path,
        std::sync::Arc::new(arrow::array::StringArray::from(keys.to_vec())),
        false,
    );
}

/// A group declaring neither roster form, its points behind `fields.view` — the third form of
/// `views.md` §3.1.
const MINTED: &str = r#"
[[view_group]]
name             = "quarter"
extent           = { min = -40.0, max = 40.0 }
source           = "topics"
fields           = { view = "quarter" }
point_visibility = { field = "categories", default = "public" }
"#;

/// Parse `SEVERITY` plus `MINTED` against a `topics.parquet` carrying `keys`, and return the
/// registry — or the refusal the mint made.
fn minted_registry(keys: &[&str]) -> Result<Vec<BuildView>> {
    let dir = tempfile::tempdir().expect("tempdir");
    write_discriminated(&dir.path().join("topics.parquet"), keys);
    let text = format!("{SEVERITY}{MINTED}");
    let config = parse_at(dir.path(), &text, &HashMap::new()).expect("the declaration parses");
    config.build_views()
}

/// **A group with no roster mints one view per distinct discriminator value, in key-byte order**
/// (`views.md` §3.1). The file's own order is `2026-Q3, 2026-Q1, 2026-Q3, 2026-Q2` and the roster
/// is not: roster order is served order (decision 0113), so a mint that took appearance order
/// would make the served order a property of how the source's rows happen to be arranged.
#[test]
fn a_group_with_no_roster_mints_its_views_from_the_discriminator() {
    let registry =
        minted_registry(&["2026-Q3", "2026-Q1", "2026-Q3", "2026-Q2"]).expect("the mint succeeds");
    let minted: Vec<&str> = registry
        .iter()
        .filter(|v| v.group.is_some())
        .map(|v| v.id.as_str())
        .collect();
    assert_eq!(
        minted,
        ["quarter:2026-Q1", "quarter:2026-Q2", "quarter:2026-Q3"]
    );
    // Every minted view is an ordinary roster record: the group's own source selected by the
    // discriminator, no metadata, and the group's own gate.
    for view in registry.iter().filter(|v| v.group.is_some()) {
        let membership = view.group.as_ref().expect("a minted view is a group's");
        assert_eq!(membership.group, "quarter");
        assert!(membership.metadata.is_empty(), "a minted view carries none");
        assert_eq!(view.visibility, None, "the group's own gate, which is public");
        let select = view.select.as_ref().expect("selected by the discriminator");
        assert_eq!(select.column, "quarter");
        assert_eq!(select.value, membership.key);
        assert_eq!(select.keys, ["2026-Q1", "2026-Q2", "2026-Q3"]);
    }
}

/// **A distinct value that cannot be a key is a refusal naming the value and the column**, not a
/// skip and not a mangling: a value no view was minted for is one whose rows belong to no view.
#[test]
fn a_minted_key_outside_the_charset_is_refused() {
    let message = format!(
        "{}",
        minted_registry(&["2026-Q1", "2026 Q2"]).expect_err("expected a refusal")
    );
    assert!(message.contains("'2026 Q2'"), "{message}");
    assert!(message.contains("column 'quarter'"), "{message}");
    assert!(message.contains("ASCII letters"), "{message}");
}

/// **A source with no rows mints no views**, and a group with none is a declaration promising
/// coordinate systems the bundle would not carry — the refusal a roster table with no rows earns.
#[test]
fn a_group_with_no_roster_and_no_rows_is_refused() {
    let message = format!(
        "{}",
        minted_registry(&[]).expect_err("expected a refusal")
    );
    assert!(message.contains("carries no rows"), "{message}");
    assert!(message.contains("view group 'quarter'"), "{message}");
}

/// A `members` group whose **owner** declares no roster: the only arrangement where the mint runs
/// against a file that is not the minting group's own.
const MINTED_MEMBERS: &str = r#"
[[view_group]]
name             = "quarter_map"
members          = "quarter"
extent           = { min = -40.0, max = 40.0 }
source           = "other"
fields           = { view = "quarter" }
point_visibility = { field = "categories", default = "public" }
"#;

/// **A `members` group takes the owner's minted keys** (`views.md` §3.3): the keys are read from
/// the *owner's* points, and this group's own points are its own file, selected by the same
/// discriminator. Two layouts over one key set, neither of which declared the set.
#[test]
fn a_members_group_takes_the_owners_minted_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The owner's file names the keys; the member's carries the same keys in another order and in
    // another layout, as a second layout over one key set does.
    write_discriminated(&dir.path().join("topics.parquet"), &["2026-Q2", "2026-Q1"]);
    write_discriminated(&dir.path().join("other.parquet"), &["2026-Q1", "2026-Q2"]);
    let text = format!("{SEVERITY}{MINTED}{MINTED_MEMBERS}");
    let config = parse_at(dir.path(), &text, &HashMap::new()).expect("both groups parse");
    let registry = config.build_views().expect("the owner's roster is minted once");
    assert_eq!(
        registry.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        [
            "s0",
            "quarter:2026-Q1",
            "quarter:2026-Q2",
            "quarter_map:2026-Q1",
            "quarter_map:2026-Q2"
        ]
    );
    // Each group's points are its own file, and both select on the keys the owner's file named.
    for view in registry.iter().filter(|v| v.group.is_some()) {
        let membership = view.group.as_ref().expect("a group's view");
        let file = match membership.group.as_str() {
            "quarter" => "topics.parquet",
            _ => "other.parquet",
        };
        assert_eq!(view.source.as_deref(), Some(dir.path().join(file).as_path()));
        let select = view.select.as_ref().expect("selected by the discriminator");
        assert_eq!(select.keys, ["2026-Q1", "2026-Q2"]);
    }
}

/// **A null in the discriminator is refused**: a row that names no view is in no view, and the
/// mint is the first reader to see it.
#[test]
fn a_null_discriminator_is_refused_at_the_mint() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_discriminator(
        &dir.path().join("topics.parquet"),
        std::sync::Arc::new(arrow::array::StringArray::from(vec![
            Some("2026-Q1"),
            None,
        ])),
        true,
    );
    let text = format!("{SEVERITY}{MINTED}");
    let message = format!(
        "{}",
        parse_at(dir.path(), &text, &HashMap::new())
            .expect("the declaration parses")
            .build_views()
            .expect_err("expected a refusal")
    );
    assert!(message.contains("has a null in it"), "{message}");
    assert!(message.contains("a row that names no view is in no view"), "{message}");
}

/// **A discriminator that is not a string is refused, not coerced**: a key read out of another
/// type would mint a view under a name nobody wrote (`views.md` §3.2's charset).
#[test]
fn a_discriminator_of_another_type_is_refused_at_the_mint() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_discriminator(
        &dir.path().join("topics.parquet"),
        std::sync::Arc::new(arrow::array::Int64Array::from(vec![1_i64, 2])),
        false,
    );
    let text = format!("{SEVERITY}{MINTED}");
    let message = format!(
        "{}",
        parse_at(dir.path(), &text, &HashMap::new())
            .expect("the declaration parses")
            .build_views()
            .expect_err("expected a refusal")
    );
    assert!(message.contains("has type Int64"), "{message}");
    assert!(message.contains("a view key is a string"), "{message}");
}
