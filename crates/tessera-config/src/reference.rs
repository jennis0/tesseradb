//! `docs/reference/tessera-toml.md`, rendered from the doc comments in `lib.rs`.
//!
//! The page is committed so the documentation site builds without cargo. The first test below
//! renders it again and fails when the committed copy differs. After changing a key or its doc
//! comment, regenerate the page with:
//!
//! ```text
//! TESSERA_WRITE_CONFIG_REFERENCE=1 cargo test -p tessera-config config_reference
//! ```
//!
//! Each table of the file is a field of `RawConfig`, and each row a field of that table's struct,
//! described by its doc comment as `tessera_docgen` reads one. The other tests hold the page to
//! `parse`: it lists exactly the keys `parse` accepts, each default it writes as a TOML value
//! parses to the same `Config` as leaving the key out, and each key it calls required is refused
//! when absent.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use tessera_docgen::{Doc, Key, Source, TABLE_HEADER};

use super::*;

const REGENERATE: &str =
    "TESSERA_WRITE_CONFIG_REFERENCE=1 cargo test -p tessera-config config_reference";

/// The smallest file `parse` accepts.
const MINIMAL: &str = "\
[bundle]
path = \"b\"
cache = \"c\"
wal = \"w\"

[plugin]
module = \"builtin:passthrough\"

[disclosure]
token_max_lifetime = 3600
";

/// `[identity]` is read by hand in `parse`, so no struct there names its keys. This one does, for
/// the page and for the tests below.
#[allow(dead_code)]
struct RawIdentity {
    /// The environment variable holding the identity key. `tessera build` also reads it from a
    /// `.env` file beside `tessera.toml`, and the process environment takes precedence over the
    /// file. An empty value takes the default.
    ///
    /// Default: `"TESSERA_IDENTITY_KEY"`.
    env: Option<String>,
}

/// One table of the file.
struct Section {
    name: String,
    /// The `RawConfig` field's doc comment.
    doc: Doc,
    required: bool,
    keys: Vec<Key>,
}

fn crate_file(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn sections() -> (Doc, Vec<Section>) {
    let lib = Source::read(&crate_file("src/lib.rs"));
    let own = Source::read(&crate_file("src/reference.rs"));
    let sections = lib
        .keys("RawConfig")
        .into_iter()
        .map(|field| {
            let keys = match field.type_name() {
                Some((name, _)) if lib.has_struct(&name) => lib.keys(&name),
                _ if field.name == "identity" => own.keys("RawIdentity"),
                _ => panic!("RawConfig.{} is not a table this page knows", field.name),
            };
            Section {
                required: field.doc.required || (!field.optional && !field.serde_default),
                name: field.name,
                doc: field.doc,
                keys,
            }
        })
        .collect();
    (lib.struct_doc("RawConfig"), sections)
}

/// The whole page.
fn render() -> String {
    let (intro, sections) = sections();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<!-- Generated from crates/tessera-config/src/lib.rs. Edit the doc comments there, then \
         run: {REGENERATE} -->\n"
    );
    out.push_str("# tessera.toml\n\n");
    let _ = writeln!(out, "{}", intro.markdown());
    let mut problems = Vec::new();
    for section in &sections {
        let _ = writeln!(out, "\n## `[{}]`\n", section.name);
        let _ = writeln!(out, "{}", section.doc.markdown());
        if section.required {
            out.push_str("\nThe table is required.\n");
        }
        let _ = writeln!(out, "\n{TABLE_HEADER}");
        for key in &section.keys {
            match key.row(&|_| false) {
                Ok(row) => {
                    let _ = writeln!(out, "{row}");
                }
                Err(problem) => problems.push(format!("[{}] {problem}", section.name)),
            }
        }
    }
    assert!(
        problems.is_empty(),
        "the page cannot be rendered:\n{}",
        problems.join("\n")
    );
    out
}

/// `text` with `line` written into `[section]`, the table added at the end if `text` has none.
fn with_line(text: &str, section: &str, line: &str) -> String {
    let header = format!("[{section}]\n");
    match text.find(&header) {
        Some(at) => {
            let at = at + header.len();
            format!("{}{line}\n{}", &text[..at], &text[at..])
        }
        None => format!("{text}\n{header}{line}\n"),
    }
}

/// `text` with `key`'s line removed from `[section]`, or with the whole table removed when `key`
/// is `None`.
fn without(text: &str, section: &str, key: Option<&str>) -> String {
    let header = format!("[{section}]");
    let mut inside = false;
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with('[') {
            inside = line == header;
        }
        let dropped = inside
            && match key {
                None => true,
                Some(key) => line
                    .split_once('=')
                    .is_some_and(|(name, _)| name.trim() == key),
            };
        if !dropped {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn refusal(text: &str) -> String {
    match parse(text) {
        Ok(_) => panic!("parse accepted:\n{text}"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn config_reference_matches_the_source() {
    tessera_docgen::check_page(
        Path::new(&crate_file("../../docs/reference/tessera-toml.md")),
        &render(),
        "TESSERA_WRITE_CONFIG_REFERENCE",
        REGENERATE,
    );
}

#[test]
fn the_reference_lists_the_keys_parse_accepts() {
    let (_, sections) = sections();
    let mut tables: Vec<String> = sections.iter().map(|s| s.name.clone()).collect();
    tables.sort();
    let accepted = tessera_docgen::accepted_keys(&refusal(&format!("nonesuch = 1\n{MINIMAL}")));
    assert_eq!(accepted, Some(tables), "the file's tables");

    for section in &sections {
        let probe = with_line(MINIMAL, &section.name, "nonesuch = 1");
        let mut keys: Vec<String> = section.keys.iter().map(|k| k.name.clone()).collect();
        keys.sort();
        if section.name == "identity" {
            // Read by hand, so the refusal names the key it does not know and lists nothing.
            assert!(matches!(
                parse(&probe),
                Err(ConfigError::UnknownIdentityKey(_))
            ));
            for key in &keys {
                parse(&with_line(MINIMAL, "identity", &format!("{key} = \"X\"")))
                    .unwrap_or_else(|e| panic!("[identity] {key}: {e}"));
            }
            continue;
        }
        let accepted = tessera_docgen::accepted_keys(&refusal(&probe));
        assert_eq!(accepted, Some(keys), "[{}]", section.name);
    }
}

#[test]
fn each_default_the_reference_states_is_the_one_parse_applies() {
    let (_, sections) = sections();
    let absent = format!("{:?}", parse(MINIMAL).expect("the minimal file parses"));
    for section in &sections {
        for key in &section.keys {
            let Some(value) = key.stated_value() else {
                continue;
            };
            let stated = with_line(MINIMAL, &section.name, &format!("{} = {value}", key.name));
            let stated = parse(&stated)
                .unwrap_or_else(|e| panic!("[{}] {} = {value}: {e}", section.name, key.name));
            assert_eq!(
                format!("{stated:?}"),
                absent,
                "[{}] {}: the page states the default {value}, and parse applies another. \
                 Correct the doc comment's `Default:` paragraph, then regenerate the page",
                section.name,
                key.name
            );
        }
    }
}

#[test]
fn each_key_the_reference_requires_is_refused_when_absent() {
    let (_, sections) = sections();
    for section in &sections {
        if section.required {
            refusal(&without(MINIMAL, &section.name, None));
        }
        for key in section.keys.iter().filter(|k| k.doc.required) {
            refusal(&without(MINIMAL, &section.name, Some(&key.name)));
        }
    }
}
