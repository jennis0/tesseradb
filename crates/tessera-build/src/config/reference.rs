//! `docs/reference/corpus-toml.md`, rendered from the doc comments on the blocks in `config.rs`.
//!
//! The page is committed so the documentation site builds without cargo. The first test below
//! renders it again and fails when the committed copy differs. After changing a key or its doc
//! comment, regenerate the page with:
//!
//! ```text
//! TESSERA_WRITE_CORPUS_REFERENCE=1 cargo test -p tessera-build corpus_reference
//! ```
//!
//! The page walks `ConfigFile`: a field whose type is a block struct becomes a table of its own,
//! headed by its path, and each row is a field described by its doc comment as `tessera_docgen`
//! reads one. The other tests hold the page to `Config::parse`, each through [`FIXTURE`]: the page
//! lists exactly the keys the parser accepts, each default it writes as a TOML value compiles to
//! the same declaration as leaving the key out, and each key it calls required is refused when
//! absent.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use tessera_docgen::{Doc, Key, Source, TABLE_HEADER};

use super::*;

const REGENERATE: &str =
    "TESSERA_WRITE_CORPUS_REFERENCE=1 cargo test -p tessera-build corpus_reference";

/// Fields held as a `toml::Value` whose table form is a struct: the key it is under, and the
/// struct that describes the table.
const TABLE_FORMS: [(&str, &str, &str); 3] = [
    ("ViewBlock", "extent", "ExtentTable"),
    ("ViewGroupBlock", "extent", "ExtentTable"),
    ("ViewGroupBlock", "view", "RosterViewBlock"),
];

// `compile_roster_view` reads a `[[view_group.view]]` block by hand, because its keys are
// `ROSTER_KEYS` and the group's metadata names. This struct describes the fixed keys for the page,
// and a test holds it to `ROSTER_KEYS`.
/// One view of a group, written in the declaration. Beside these keys the block takes one for
/// each name the group's `metadata` declares, holding a value of the declared type, and every
/// view carries every name. A `timestamp_us` value is an offset date-time, such as
/// `2026-04-01T00:00:00Z`, or microseconds since the Unix epoch; an integer outside its type's
/// range is refused. A group-level key, such as `extent` or `projection`, is refused, and so is
/// any other key.
#[allow(dead_code)]
struct RosterViewBlock {
    /// The view's key: its id is `<group>:<key>`. ASCII letters, digits, `_` and `-`, and no two
    /// views of the group may share one.
    key: String,
    /// A name in `[sources]`: the file holding this view's points. A build refuses a view with
    /// none.
    ///
    /// Default: not set.
    source: Option<String>,
    /// The label, or list of labels of which a viewer must hold one, to reach this view as well
    /// as the group. Without it, or with `public` alone, the group's `visibility` is the view's
    /// only gate.
    ///
    /// Type: string or array of strings.
    ///
    /// Default: not set.
    visibility: Option<toml::Value>,
}

/// One table of the declaration.
struct Table {
    /// Its path, as a table header writes it: `view`, `view.extent`, `layer.content.supplied`.
    path: String,
    array: bool,
    doc: Doc,
    keys: Vec<Key>,
}

/// The two files the page is read from: `config.rs`, and this one for [`RosterViewBlock`].
struct Files {
    config: Source,
    own: Source,
}

impl Files {
    fn read() -> Files {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        Files {
            config: Source::read(&dir.join("config.rs")),
            own: Source::read(&dir.join("config/reference.rs")),
        }
    }

    fn of(&self, name: &str) -> &Source {
        if self.config.has_struct(name) {
            &self.config
        } else {
            &self.own
        }
    }

    fn is_table(&self, name: &str) -> bool {
        self.config.has_struct(name) || self.own.has_struct(name)
    }

    /// The struct a key's value is written with, and whether the file writes an array of them.
    fn child(&self, parent: &str, key: &Key) -> Option<(String, bool)> {
        if let Some((_, _, table)) = TABLE_FORMS
            .iter()
            .find(|(owner, field, _)| *owner == parent && *field == key.name)
        {
            let array = key.type_name().is_some_and(|(_, array)| array);
            return Some((table.to_string(), array));
        }
        key.type_name().filter(|(name, _)| self.is_table(name))
    }
}

/// Every table under `ConfigFile`, parents before their children, each struct once: a struct
/// reached a second time, as `[layer.labels.members]` reaches `MembersBlock`, is described where
/// it was first reached.
fn tables(sources: &Files) -> Vec<Table> {
    fn walk(
        sources: &Files,
        parent: &str,
        prefix: &str,
        seen: &mut Vec<String>,
        out: &mut Vec<Table>,
    ) {
        for key in sources.of(parent).keys(parent) {
            let Some((name, array)) = sources.child(parent, &key) else {
                continue;
            };
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let path = format!("{prefix}{}", key.name);
            out.push(Table {
                path: path.clone(),
                array,
                doc: sources.of(&name).struct_doc(&name),
                keys: sources.of(&name).keys(&name),
            });
            walk(sources, &name, &format!("{path}."), seen, out);
        }
    }
    let mut out = Vec::new();
    walk(sources, "ConfigFile", "", &mut Vec::new(), &mut out);
    out
}

/// What each declarable type holds. The match is exhaustive, so a new `ScalarType` does not
/// compile here until it is described.
fn type_description(ty: ScalarType) -> &'static str {
    match ty {
        ScalarType::Bool => "`true` or `false`",
        ScalarType::U8 => "an integer from 0 to 255",
        ScalarType::U16 => "an integer from 0 to 65,535",
        ScalarType::U32 => "an integer from 0 to 4,294,967,295",
        ScalarType::U64 => "an integer from 0 to 2^64 - 1",
        ScalarType::I8 => "an integer from -128 to 127",
        ScalarType::I16 => "an integer from -32,768 to 32,767",
        ScalarType::I32 => "a signed 32-bit integer",
        ScalarType::I64 => "a signed 64-bit integer",
        ScalarType::F32 => "a 32-bit floating-point number",
        ScalarType::F64 => "a 64-bit floating-point number",
        ScalarType::TimestampUs => "an instant, stored as microseconds since the Unix epoch",
        ScalarType::Utf8 => unreachable!("`utf8` is not a declarable type"),
        ScalarType::Keyword => {
            "a short string matched exactly, such as an identifier or a hostname. Refused with \
             `render = true`"
        }
        ScalarType::Text => {
            "prose, searched by the terms its `analyser` produces. Refused with `render = true`"
        }
    }
}

/// The table of `[[attribute]]` types, from the list the declaration's own refusal names.
fn types_table() -> String {
    let mut out = String::from("| Type | Holds |\n| --- | --- |\n");
    for name in DECLARABLE_TYPES.replace(" and ", ", ").split(", ") {
        let holds = match name {
            "category" => {
                "a value of the vocabulary `vocabulary` names, stored as its code at the \
                 vocabulary's `width`"
            }
            _ => type_description(
                ScalarType::parse(name)
                    .unwrap_or_else(|| panic!("DECLARABLE_TYPES names '{name}', not a type")),
            ),
        };
        let _ = writeln!(out, "| `{name}` | {} |", tessera_docgen::cell(holds));
    }
    out
}

/// The whole page.
fn render() -> String {
    let sources = Files::read();
    let is_table = |name: &str| sources.is_table(name);
    let mut problems = Vec::new();
    let mut rows = |path: &str, keys: &[Key], out: &mut String| {
        let _ = writeln!(out, "{TABLE_HEADER}");
        for key in keys {
            match key.row(&is_table) {
                Ok(row) => {
                    let _ = writeln!(out, "{row}");
                }
                Err(problem) => problems.push(format!("{path}: {problem}")),
            }
        }
    };

    let mut out = String::new();
    let _ = writeln!(
        out,
        "<!-- Generated from crates/tessera-build/src/config.rs. Edit the doc comments there, \
         then run: {REGENERATE} -->\n"
    );
    out.push_str("# corpus.toml\n\n");
    let _ = writeln!(
        out,
        "{}\n",
        sources.config.struct_doc("ConfigFile").markdown()
    );
    rows(
        "the top level",
        &sources.config.keys("ConfigFile"),
        &mut out,
    );
    for table in tables(&sources) {
        let header = match table.array {
            true => format!("[[{}]]", table.path),
            false => format!("[{}]", table.path),
        };
        let _ = writeln!(out, "\n## `{header}`\n");
        let _ = writeln!(out, "{}\n", table.doc.markdown());
        rows(&table.path, &table.keys, &mut out);
        if table.path == "attribute" {
            let _ = writeln!(
                out,
                "\nThe types `type` takes:\n\n{}",
                types_table().trim_end()
            );
        }
    }
    assert!(
        problems.is_empty(),
        "the page cannot be rendered:\n{}",
        problems.join("\n")
    );
    out
}

#[test]
fn corpus_reference_matches_the_source() {
    tessera_docgen::check_page(
        Path::new(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/corpus-toml.md"),
        ),
        &render(),
        "TESSERA_WRITE_CORPUS_REFERENCE",
        REGENERATE,
    );
}

// ---------------------------------------------------------------------------------------------
// The page against the parser
// ---------------------------------------------------------------------------------------------

/// A declaration with every table in it, each key on a line of its own so a test can add or
/// remove one. Parsed as a build parses it; nothing here reads the files `[sources]` names.
const FIXTURE: &str = r#"[sources]
points        = "points.parquet"
eras_points   = "eras_points.parquet"
eras_roster   = "eras_roster.parquet"
topics        = "topics.parquet"
topic_members = "topic_members.parquet"
topic_labels  = "topic_labels.parquet"
zones         = "zones.parquet"

[defaults]
source = "points"

[[view]]
name = "map"
extent.auto = true
point_visibility.default = "public"

[[view_group]]
name = "eras"
source = "eras_points"
extent.auto = true
point_visibility.default = "public"

[view_group.views]
source = "eras_roster"

[[vocabulary]]
name = "severity"
width = "u8"
value_set = "closed"
visibility = "public"
values.low = 1

[[attribute]]
name = "severity"
type = "category"
vocabulary = "severity"
index = true

[[attribute]]
name = "body"
type = "text"
index = true

[[layer]]
name = "zones"
views = ["map"]
source = "zones"
membership = "spatial"
hierarchy.kind = "flat"
hierarchy.prune_children = true
visibility = "public"
artifact_visibility.default = "inherited"
require_member_visibility = "none"
shape.kind = "bbox"

[[layer]]
name = "regions"
views = ["map"]
membership = "spatial"
hierarchy.kind = "flat"
visibility = "public"
artifact_visibility.default = "inherited"
require_member_visibility = "none"
shape.kind = "bbox"

[[layer.artifacts]]
key = "north"
bbox = [0.0, 0.0, 1.0, 1.0]

[[layer]]
name = "topics"
views = ["map"]
source = "topics"
membership = "enumerated"
hierarchy.kind = "stacked"
visibility = "staff"
artifact_visibility.default = "inherited"
require_member_visibility = "any"

[[layer.levels]]
level = 0

[layer.content]
computed = ["centroid"]

[[layer.content.supplied]]
name = "summary"
type = "text"
require_member_visibility = "all"

[layer.members]
source = "topic_members"

[layer.labels]
name = "topic_labels"
source = "topic_labels"
type = "text"
membership = "enumerated"
require_member_visibility = "any"
artifact_visibility.default = "inherited"
content.require_member_visibility = "all"
"#;

/// Where a table's keys are written in [`FIXTURE`]: after `anchor`, each key behind `prefix`.
fn place(path: &str) -> (&'static str, &'static str) {
    // A spatial layer reading a file, the one on which `default_space` has an effect.
    const ZONES: &str = "[[layer]]\nname = \"zones\"\n";
    match path {
        "defaults" => ("[defaults]\n", ""),
        "view" => ("[[view]]\n", ""),
        "view.extent" => ("[[view]]\n", "extent."),
        "view.point_visibility" => ("[[view]]\n", "point_visibility."),
        "view_group" => ("[[view_group]]\n", ""),
        "view_group.views" => ("[view_group.views]\n", ""),
        "vocabulary" => ("[[vocabulary]]\n", ""),
        // The text column, the one `analyser` may be written on.
        "attribute" => ("[[attribute]]\nname = \"body\"\n", ""),
        "layer" => (ZONES, ""),
        "layer.artifacts" => ("[[layer.artifacts]]\n", ""),
        "layer.members" => ("[layer.members]\n", ""),
        "layer.hierarchy" => (ZONES, "hierarchy."),
        "layer.levels" => ("[[layer.levels]]\n", ""),
        "layer.artifact_visibility" => (ZONES, "artifact_visibility."),
        "layer.content" => ("[layer.content]\n", ""),
        "layer.content.supplied" => ("[[layer.content.supplied]]\n", ""),
        "layer.shape" => (ZONES, "shape."),
        "layer.labels" => ("[layer.labels]\n", ""),
        "layer.labels.content" => ("[layer.labels]\n", "content."),
        other => panic!("no place in FIXTURE for `{other}`; add one to `place`"),
    }
}

/// `text`, [`FIXTURE`] or a variant of it, with `line` written into the table at `path`.
fn with_line(text: &str, path: &str, line: &str) -> String {
    let (anchor, prefix) = place(path);
    let at = text.find(anchor).expect("the anchor is in the text") + anchor.len();
    format!("{}{prefix}{line}\n{}", &text[..at], &text[at..])
}

/// `text`, [`FIXTURE`] or a variant of it, with `key`'s lines removed from the table at `path`.
/// The table itself stays: a table written as dotted keys that loses its last key is written
/// `table = {}`.
fn without(text: &str, path: &str, key: &str) -> String {
    let (anchor, prefix) = place(path);
    let at = text.find(anchor).expect("the anchor is in the text") + anchor.len();
    let (before, after) = text.split_at(at);
    let written = format!("{prefix}{key}");
    let mut inside = true;
    let mut kept = Vec::new();
    let mut table_left = false;
    for line in after.lines() {
        inside &= !line.starts_with('[');
        let name = line.split_once('=').map(|(name, _)| name.trim());
        if inside && name.is_some_and(|n| n == written || n.starts_with(&format!("{written}."))) {
            continue;
        }
        table_left |= inside && name.is_some_and(|n| n.starts_with(prefix));
        kept.push(line);
    }
    let mut out = before.to_string();
    if !prefix.is_empty() && !table_left {
        let _ = writeln!(out, "{} = {{}}", prefix.trim_end_matches('.'));
    }
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `text` compiled as a build compiles it, from `corpus.toml` in `dir`.
fn parsed_in(dir: &Path, text: &str) -> Result<Config> {
    let path = dir.join("corpus.toml");
    std::fs::write(&path, text).expect("the declaration is writable");
    Config::parse(&path, &HashMap::new())
}

fn parsed(text: &str) -> Result<Config> {
    parsed_in(
        tempfile::tempdir().expect("a temporary directory").path(),
        text,
    )
}

fn refusal(text: &str) -> String {
    match parsed(text) {
        Ok(_) => panic!("the declaration was accepted:\n{text}"),
        Err(e) => e.to_string(),
    }
}

/// A compiled declaration as text two compilations can be compared by. `Debug` prints a hash map
/// in an order that differs between two maps of the same contents, so the vocabularies, the one
/// hash map in a `Config`, are printed by name and by code first, and left out of the rest.
fn canonical(mut config: Config) -> String {
    let vocabularies: BTreeMap<String, String> = config
        .schema
        .vocabularies
        .drain()
        .map(|(name, v)| {
            let values: Vec<(&str, u32, Option<&str>)> = v
                .values
                .bindings()
                .map(|(key, code)| (key, code, v.values.title_of(key)))
                .collect();
            let text = format!(
                "{:?} {:?} {:?} {:?} {:?} {:?} {values:?}",
                v.title,
                v.value_set,
                v.width,
                v.reserved,
                v.values.kind(),
                v.values.visibility()
            );
            (name, text)
        })
        .collect();
    format!("{vocabularies:?} {config:?}")
}

/// Keys whose stated default the compiled declaration does not record. An inline artifact row is
/// kept as written, and its `space` is applied when the build reads the row, so
/// `the_space_of_an_inline_row_defaults_to_the_one_the_page_states` checks it there.
const APPLIED_AT_BUILD: [(&str, &str); 1] = [("layer.artifacts", "space")];

#[test]
fn the_fixture_is_a_declaration() {
    parsed(FIXTURE).expect("FIXTURE compiles");
}

#[test]
fn the_reference_lists_the_keys_the_parser_accepts() {
    let sources = Files::read();
    let mut top: Vec<String> = sources
        .config
        .keys("ConfigFile")
        .into_iter()
        .map(|k| k.name)
        .collect();
    top.sort();
    let accepted = tessera_docgen::accepted_keys(&refusal(&format!("nonesuch = 1\n{FIXTURE}")));
    assert_eq!(accepted, Some(top), "the top level");

    for table in tables(&sources) {
        let mut keys: Vec<String> = table.keys.iter().map(|k| k.name.clone()).collect();
        keys.sort();
        if table.path == "view_group.view" {
            // Read by hand against `ROSTER_KEYS` and the group's metadata names.
            let mut roster: Vec<String> = ROSTER_KEYS.iter().map(|k| k.to_string()).collect();
            roster.sort();
            assert_eq!(keys, roster, "[[view_group.view]]");
            continue;
        }
        let message = refusal(&with_line(FIXTURE, &table.path, "nonesuch = 1"));
        let accepted = tessera_docgen::accepted_keys(&message);
        assert_eq!(accepted, Some(keys), "{}: {message}", table.path);
    }
}

#[test]
fn each_default_the_reference_states_is_the_one_the_parser_applies() {
    for table in tables(&Files::read()) {
        for key in &table.keys {
            let Some(value) = key.stated_value() else {
                continue;
            };
            if APPLIED_AT_BUILD.contains(&(table.path.as_str(), key.name.as_str())) {
                continue;
            }
            let absent = without(FIXTURE, &table.path, &key.name);
            let stated = with_line(&absent, &table.path, &format!("{} = {value}", key.name));
            // One directory for both, since each source's resolved path is part of the declaration.
            let dir = tempfile::tempdir().expect("a temporary directory");
            let compiled = |text: &str| {
                canonical(
                    parsed_in(dir.path(), text)
                        .unwrap_or_else(|e| panic!("{}.{}: {e}", table.path, key.name)),
                )
            };
            assert_eq!(
                compiled(&stated),
                compiled(&absent),
                "{}.{}: the page states the default {value}, and the parser applies another. \
                 Correct the doc comment's `Default:` paragraph, then regenerate the page",
                table.path,
                key.name
            );
        }
    }
}

#[test]
fn each_key_the_reference_requires_is_refused_when_absent() {
    for table in tables(&Files::read()) {
        for key in table.keys.iter().filter(|k| k.doc.required) {
            let absent = without(FIXTURE, &table.path, &key.name);
            assert_ne!(
                absent, FIXTURE,
                "{}.{} is not in FIXTURE",
                table.path, key.name
            );
            assert!(
                parsed(&absent).is_err(),
                "{}.{}: the page calls it required, and a declaration without it compiles",
                table.path,
                key.name
            );
        }
    }
}

#[test]
fn the_space_of_an_inline_row_defaults_to_the_one_the_page_states() {
    let files = Files::read();
    let stated = files
        .config
        .keys("InlineArtifact")
        .into_iter()
        .find(|k| k.name == "space")
        .and_then(|k| k.stated_value().map(str::to_string))
        .expect("the page states a default `space`");
    let stated: toml::Value = toml::from_str(&format!("space = {stated}")).expect("a TOML value");
    let stated = stated["space"].as_str().expect("a string").to_string();

    let config = parsed(FIXTURE).expect("FIXTURE compiles");
    let frame = tessera_store::derived::ViewFrame::new(
        "map",
        Projection::None,
        Bounds {
            x_min: 0.0,
            x_max: 16.0,
            y_min: 0.0,
            y_max: 16.0,
        },
    );
    // The `regions` layer's rows, as the build reads them with `space` set to `space`.
    let read = |space: Option<&str>| {
        let mut inputs: Vec<LayerSources> = config
            .layer_sources
            .iter()
            .filter(|s| s.name == "regions")
            .cloned()
            .collect();
        let Some(ArtifactSource::Inline(rows)) = &mut inputs[0].artifacts else {
            panic!("`regions` writes its artifacts inline");
        };
        for row in rows.iter_mut() {
            row.space = space.map(str::to_string);
        }
        let scratch = tempfile::tempdir().expect("a temporary directory");
        crate::layers::read(
            &config.layers,
            &inputs,
            &crate::ids::IdSpace::Integer { signed: false },
            &BTreeMap::new(),
            std::slice::from_ref(&frame),
            tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES,
            scratch.path(),
            1 << 30,
        )
        .map(|plan| plan.shape_reports)
        .unwrap_or_else(|e| panic!("reading `regions` with space {space:?}: {e}"))
    };
    assert_eq!(
        read(None),
        read(Some(&stated)),
        "an inline row without `space` is not read as `space = \"{stated}\"`"
    );
}

/// The keys whose default the page states in words, each checked by the test below.
const WORDED: [&str; 6] = [
    "view.source",
    "attribute.field",
    "attribute.source",
    "layer.layout",
    "layer.levels.zoom",
    "layer.labels.visibility",
];

#[test]
fn each_default_stated_in_words_is_the_one_the_parser_applies() {
    let worded: Vec<String> = tables(&Files::read())
        .iter()
        .flat_map(|table| {
            table
                .keys
                .iter()
                .filter(|k| {
                    k.doc
                        .default
                        .as_deref()
                        .is_some_and(|d| !d.starts_with('`') && d != "not set")
                })
                .map(|k| format!("{}.{}", table.path, k.name))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        worded, WORDED,
        "each default stated in words needs a check in this test"
    );

    let dir = tempfile::tempdir().expect("a temporary directory");
    let config = parsed_in(dir.path(), FIXTURE).expect("FIXTURE compiles");
    // `[defaults].source` is `points`, and the view and the attributes name no source.
    assert_eq!(
        config.views[0].source,
        Some(dir.path().join("points.parquet"))
    );
    assert_eq!(config.attribute_sources.len(), 1);
    assert_eq!(config.attribute_sources[0].name, "points");
    for attribute in &config.schema.attributes {
        assert_eq!(attribute.column(), attribute.name);
    }
    assert!(config.layers.iter().all(|layer| layer.layout.is_none()));
    let topics = config.layers.iter().find(|l| l.name == "topics").unwrap();
    assert!(topics.levels.iter().all(|level| level.zoom.is_none()));
    let labels = config
        .layers
        .iter()
        .find(|l| l.name == "topic_labels")
        .unwrap();
    assert_eq!(topics.visibility.as_deref(), Some("staff"));
    assert_eq!(labels.visibility, topics.visibility);
}
