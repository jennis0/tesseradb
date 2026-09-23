//! `docs/reference/cli.md`, rendered from the command definitions in `main.rs`.
//!
//! The page is committed so the documentation site builds without cargo. The test below renders
//! it again and fails when the committed copy differs. To regenerate the page after changing a
//! command or its help text, run:
//!
//! ```text
//! TESSERA_WRITE_CLI_REFERENCE=1 cargo test -p tessera-cli cli_reference
//! ```

use std::fmt::Write;
use std::path::PathBuf;

use clap::{Arg, ArgAction, Command, CommandFactory};

const REGENERATE: &str = "TESSERA_WRITE_CLI_REFERENCE=1 cargo test -p tessera-cli cli_reference";

const INTRO: &str = "\
`tessera` is one binary with a subcommand for each job: `build` makes a bundle from a corpus \
declaration, `check` tests the declaration against its source files, `verify` checks a built \
bundle, `tokenise` shows how an analyser splits text, and `serve` serves the bundle. \
`tessera <subcommand> --help` prints the text on this page, and `tessera --version` prints the \
commit the binary was built from.

`build`, `check` and `serve` read the deployment file `tessera.toml` from the working directory, \
or from the nearest directory above it that has one. `--deployment` names a different file.
";

fn page_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/cli.md")
}

/// The whole page.
pub(crate) fn render() -> String {
    let mut cli = crate::Cli::command();
    cli.build();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<!-- Generated from crates/tessera-cli/src/main.rs. Edit the help text there, then run: \
         {REGENERATE} -->\n"
    );
    out.push_str("# CLI\n\n");
    out.push_str(INTRO);
    let names: Vec<String> = cli
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        let sub = cli
            .find_subcommand_mut(&name)
            .expect("a name read from this command's own subcommands");
        section(&mut out, sub);
    }
    out
}

fn section(out: &mut String, sub: &mut Command) {
    let usage = sub.render_usage().to_string();
    let usage = usage.trim().trim_start_matches("Usage:").trim();
    let about = sub
        .get_long_about()
        .or(sub.get_about())
        .map(|text| sentence(&text.to_string()))
        .unwrap_or_default();
    let _ = writeln!(out, "\n## `tessera {}`\n", sub.get_name());
    let _ = writeln!(out, "```text\n{usage}\n```\n");
    let _ = writeln!(out, "{about}\n");

    let args: Vec<&Arg> = sub
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter(|arg| {
            !matches!(
                arg.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        })
        .collect();
    if args.is_empty() {
        return;
    }
    out.push_str("| Argument | Value | Default | Description |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for arg in args {
        let value = value_name(arg);
        let name = match arg.get_long() {
            Some(long) => format!("`--{long}`"),
            None => format!("`{value}`"),
        };
        let value = match (arg.get_long(), arg.get_action().takes_values()) {
            (Some(_), true) => format!("`{value}`"),
            _ => String::new(),
        };
        // A switch's default of `false` says nothing its description does not.
        let default = match arg.get_action() {
            ArgAction::SetTrue => &[][..],
            _ => arg.get_default_values(),
        }
        .iter()
        .map(|v| format!("`{}`", v.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
        let help = arg
            .get_long_help()
            .or(arg.get_help())
            .map(|text| cell(&sentence(&text.to_string())))
            .unwrap_or_default();
        let _ = writeln!(out, "| {name} | {value} | {default} | {help} |");
    }
}

fn value_name(arg: &Arg) -> String {
    match arg.get_value_names() {
        Some(names) => names
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        None => arg.get_id().as_str().to_uppercase(),
    }
}

/// Help text ending in a full stop. Clap drops the full stop from a help text of one paragraph,
/// which reads well at a terminal and badly in running prose.
fn sentence(text: &str) -> String {
    let text = text.trim();
    match text.chars().last() {
        Some('.') | Some(':') | None => text.to_string(),
        Some(_) => format!("{text}."),
    }
}

/// A help text as one table cell: its paragraphs on one line, and any `|` escaped.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

#[test]
fn cli_reference_matches_the_commands() {
    let path = page_path();
    let rendered = render();
    if std::env::var_os("TESSERA_WRITE_CLI_REFERENCE").is_some() {
        std::fs::write(&path, &rendered).expect("docs/reference/cli.md is writable");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == rendered,
        "docs/reference/cli.md differs from the commands in crates/tessera-cli/src/main.rs. \
         Regenerate it with: {REGENERATE}"
    );
}
